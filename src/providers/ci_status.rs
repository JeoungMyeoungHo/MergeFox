//! Continuous-integration check summaries per commit.
//!
//! The goal is a single badge next to each commit subject in the graph
//! telling the user at a glance whether CI is green / red / running /
//! unknown. We deliberately collapse the forge's richer per-check model
//! (GitHub's check-runs are JSON objects with conclusions / statuses /
//! per-app provenance; GitLab's pipeline statuses are their own state
//! machine) into a five-state enum so the renderer doesn't have to know
//! about provider quirks.
//!
//! # Why a dedicated module and not inline in the provider trait
//!
//! 1. The Provider trait already carries every "one repo, one
//!    account-token" shape — `discover_repo`, `list_pull_requests`, etc.
//!    Adding `check_summary_for_commit` there would force every
//!    concrete provider (Bitbucket, Azure, Gitea, Generic…) to
//!    implement or stub an endpoint that isn't supported on most of
//!    them. Keeping this outside the trait means we can expand
//!    coverage provider-by-provider without perturbing the core
//!    abstraction.
//!
//! 2. CI status is a *bulk* query (the graph wants results for dozens
//!    of commits at once). The provider trait is per-request; baking
//!    bulk semantics in here lets us pick the cheapest shape on each
//!    forge (e.g. GitHub's combined `/commits/{sha}/status` collapses
//!    all per-commit statuses into a single call; check-runs for the
//!    same sha is a second call that we merge).
//!
//! 3. No forge connection / no network / unsupported provider →
//!    return `Ok(HashMap::new())`. The UI treats absence from the map
//!    as "no badge" (not `CiStatus::Unknown`) so commits the forge
//!    never checked stay visually quiet.
//!
//! # Rate-limit safety
//!
//! GitHub's anonymous rate limit is 60 req/h; authenticated is
//! 5000 req/h. We fan out one HTTP call per commit, capped at the
//! number of rows requested (`MAX_OIDS`). The caller is responsible
//! for throttling — typically the graph-rebuild path will queue ≤ 50
//! commits and re-queue every 2 minutes, which stays comfortably under
//! the authenticated limit even for the most active repo.

use std::collections::HashMap;

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::providers::error::{ProviderError, ProviderResult};
use crate::providers::types::ProviderKind;

/// Hard ceiling on commits we'll query in one bulk call. Beyond this,
/// extra oids are silently dropped. Picked to match the graph's
/// "first N visible rows" heuristic — the user can't meaningfully
/// scan more than this at a glance anyway, and keeping it here as a
/// single constant makes the rate-limit budget obvious at review.
pub const MAX_OIDS: usize = 50;

/// Collapsed CI state. Ordered roughly by severity so comparisons
/// pick the more-alarming state when aggregating multiple checks:
/// `Failure > Pending > Neutral > Success > Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    Success,
    Failure,
    Pending,
    Neutral,
    Unknown,
}

/// Per-commit rollup. Counts are across ALL checks/statuses attached
/// to the commit at query time — the badge tooltip reads directly off
/// these, so it matches the top-level `status` field by construction.
#[derive(Debug, Clone)]
pub struct CheckSummary {
    pub oid: gix::ObjectId,
    pub status: CiStatus,
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    pub details_url: Option<String>,
}

/// Fetch check summaries for `oids` from the given forge.
///
/// Returns a map keyed by commit oid; commits with no recorded checks
/// are absent from the map (not `Unknown`). This distinction matters
/// for rendering: absent → no badge; `Unknown` → gray badge.
///
/// Non-CI-capable providers return an empty map. Networking / auth
/// failures bubble up as `ProviderError` so the caller can log + back
/// off; we never fall back to a partial cache mid-call.
pub async fn fetch_check_summaries(
    kind: &ProviderKind,
    client: &Client,
    token: Option<&SecretString>,
    owner: &str,
    repo: &str,
    oids: &[gix::ObjectId],
) -> ProviderResult<HashMap<gix::ObjectId, CheckSummary>> {
    let oids = if oids.len() > MAX_OIDS {
        &oids[..MAX_OIDS]
    } else {
        oids
    };

    match kind {
        ProviderKind::GitHub => fetch_github(client, token, owner, repo, oids).await,
        ProviderKind::GitLab => fetch_gitlab(client, token, owner, repo, oids).await,
        // Codeberg / Gitea expose a similar check-runs endpoint under
        // `/api/v1/repos/{owner}/{repo}/commits/{sha}/status`, but we
        // haven't validated the shape across self-hosted instances —
        // return empty so the UI degrades gracefully rather than
        // surfacing confusing error banners on every graph rebuild.
        _ => Ok(HashMap::new()),
    }
}

// ---------- GitHub ----------

#[derive(Deserialize)]
struct GhCombinedStatus {
    state: String,
    #[serde(default)]
    statuses: Vec<GhStatusEntry>,
    #[serde(default)]
    #[allow(dead_code)]
    total_count: u32,
}

#[derive(Deserialize)]
struct GhStatusEntry {
    state: String,
    #[serde(default)]
    target_url: Option<String>,
}

#[derive(Deserialize)]
struct GhCheckRunsResp {
    #[serde(default)]
    check_runs: Vec<GhCheckRun>,
}

#[derive(Deserialize)]
struct GhCheckRun {
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

async fn fetch_github(
    client: &Client,
    token: Option<&SecretString>,
    owner: &str,
    repo: &str,
    oids: &[gix::ObjectId],
) -> ProviderResult<HashMap<gix::ObjectId, CheckSummary>> {
    let mut out = HashMap::new();
    for oid in oids {
        let sha = oid.to_string();
        // 1) Combined commit status — the classic commit-status API.
        //    Returns `state` ∈ {success, failure, pending, error, ...}
        //    plus a per-status array. `error` is lumped into failure.
        let status_url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{sha}/status");
        let status = request_github(client, token, &status_url).await?;

        // 2) Check-runs — newer check API (actions, app-driven).
        //    Has its own state machine: `status` ∈ {queued, in_progress,
        //    completed} and `conclusion` ∈ {success, failure, neutral,
        //    cancelled, skipped, timed_out, action_required, stale}.
        let runs_url =
            format!("https://api.github.com/repos/{owner}/{repo}/commits/{sha}/check-runs");
        let runs = request_github(client, token, &runs_url).await?;

        let mut passed: u32 = 0;
        let mut failed: u32 = 0;
        let mut pending: u32 = 0;
        let mut details_url: Option<String> = None;
        let mut top_level: Option<String> = None;

        if let Some(bytes) = status {
            match serde_json::from_slice::<GhCombinedStatus>(&bytes) {
                Ok(parsed) => {
                    top_level = Some(parsed.state.clone());
                    for s in &parsed.statuses {
                        match classify_github_state(&s.state) {
                            CiStatus::Success => passed += 1,
                            CiStatus::Failure => failed += 1,
                            CiStatus::Pending => pending += 1,
                            _ => {}
                        }
                        if details_url.is_none() {
                            details_url = s.target_url.clone();
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        target = "mergefox::ci",
                        "github combined-status parse error for {sha}: {err}"
                    );
                }
            }
        }

        if let Some(bytes) = runs {
            match serde_json::from_slice::<GhCheckRunsResp>(&bytes) {
                Ok(parsed) => {
                    for r in &parsed.check_runs {
                        let cls = classify_check_run(r);
                        match cls {
                            CiStatus::Success => passed += 1,
                            CiStatus::Failure => failed += 1,
                            CiStatus::Pending => pending += 1,
                            CiStatus::Neutral => {}
                            CiStatus::Unknown => {}
                        }
                        if details_url.is_none() {
                            details_url = r.html_url.clone();
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        target = "mergefox::ci",
                        "github check-runs parse error for {sha}: {err}"
                    );
                }
            }
        }

        // Nothing recorded on either endpoint — omit this oid from
        // the map so the renderer paints no badge at all.
        if passed == 0 && failed == 0 && pending == 0 && top_level.as_deref() == Some("") {
            continue;
        }
        if passed == 0 && failed == 0 && pending == 0 && top_level.is_none() {
            continue;
        }

        let status = if failed > 0 {
            CiStatus::Failure
        } else if pending > 0 {
            CiStatus::Pending
        } else if passed > 0 {
            CiStatus::Success
        } else {
            // Combined status gave us a top-level state but no per-status
            // entries — happens when the repo has CI but nothing ran for
            // this sha. Fall back to the top-level verdict.
            top_level
                .as_deref()
                .map(classify_github_state)
                .unwrap_or(CiStatus::Unknown)
        };

        out.insert(
            *oid,
            CheckSummary {
                oid: *oid,
                status,
                passed,
                failed,
                pending,
                details_url,
            },
        );
    }
    Ok(out)
}

/// Fire a GitHub request and return the raw body bytes on 200, `None`
/// on 404 (common: repo has no CI at all, or sha isn't indexed yet),
/// and an error on anything else.
async fn request_github(
    client: &Client,
    token: Option<&SecretString>,
    url: &str,
) -> ProviderResult<Option<Vec<u8>>> {
    let mut req = client
        .get(url)
        .header("User-Agent", "mergefox")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = token {
        req = req.header("Authorization", format!("token {}", t.expose_secret()));
    }
    let resp = req.send().await?;
    match resp.status().as_u16() {
        200 => Ok(Some(resp.bytes().await?.to_vec())),
        404 => Ok(None),
        401 | 403 => Err(ProviderError::Unauthorized),
        s => Err(ProviderError::Api {
            status: s,
            body: resp.text().await.unwrap_or_default(),
        }),
    }
}

fn classify_github_state(state: &str) -> CiStatus {
    match state {
        "success" => CiStatus::Success,
        // GitHub's combined-status uses `error` for "check itself
        // crashed"; we treat it as a failure so red lights aren't
        // hidden behind a gray badge.
        "failure" | "error" => CiStatus::Failure,
        "pending" => CiStatus::Pending,
        _ => CiStatus::Unknown,
    }
}

fn classify_check_run(r: &GhCheckRun) -> CiStatus {
    // Check-runs distinguish between "still running" (`status`) and
    // "finished with what verdict" (`conclusion`). Until a run is
    // `completed`, treat it as pending regardless of conclusion.
    if r.status != "completed" {
        return CiStatus::Pending;
    }
    match r.conclusion.as_deref() {
        Some("success") => CiStatus::Success,
        // `action_required` is a user-prompt state ("click to approve
        // this workflow run"); surfacing it as failure would nag more
        // than help, so we call it pending — the run is blocked, not
        // broken.
        Some("failure") | Some("timed_out") => CiStatus::Failure,
        Some("neutral") | Some("cancelled") | Some("skipped") | Some("stale") => CiStatus::Neutral,
        Some("action_required") => CiStatus::Pending,
        _ => CiStatus::Unknown,
    }
}

// ---------- GitLab ----------

#[derive(Deserialize)]
struct GlStatus {
    status: String,
    #[serde(default)]
    target_url: Option<String>,
}

async fn fetch_gitlab(
    client: &Client,
    token: Option<&SecretString>,
    owner: &str,
    repo: &str,
    oids: &[gix::ObjectId],
) -> ProviderResult<HashMap<gix::ObjectId, CheckSummary>> {
    // GitLab addresses projects by URL-encoded `namespace/project`. A
    // literal `/` would 404, so the slash becomes `%2F`.
    let project = format!("{owner}%2F{repo}");
    let mut out = HashMap::new();
    for oid in oids {
        let sha = oid.to_string();
        let url = format!(
            "https://gitlab.com/api/v4/projects/{project}/repository/commits/{sha}/statuses"
        );
        let mut req = client.get(&url).header("User-Agent", "mergefox");
        if let Some(t) = token {
            req = req.header("PRIVATE-TOKEN", t.expose_secret());
        }
        let resp = req.send().await?;
        match resp.status().as_u16() {
            200 => {}
            404 => continue,
            401 | 403 => return Err(ProviderError::Unauthorized),
            s => {
                return Err(ProviderError::Api {
                    status: s,
                    body: resp.text().await.unwrap_or_default(),
                })
            }
        }
        let statuses: Vec<GlStatus> = match resp.json().await {
            Ok(v) => v,
            Err(err) => {
                tracing::debug!(
                    target = "mergefox::ci",
                    "gitlab status parse error for {sha}: {err}"
                );
                continue;
            }
        };
        if statuses.is_empty() {
            continue;
        }
        let mut passed: u32 = 0;
        let mut failed: u32 = 0;
        let mut pending: u32 = 0;
        let mut details_url: Option<String> = None;
        for s in &statuses {
            match classify_gitlab_state(&s.status) {
                CiStatus::Success => passed += 1,
                CiStatus::Failure => failed += 1,
                CiStatus::Pending => pending += 1,
                _ => {}
            }
            if details_url.is_none() {
                details_url = s.target_url.clone();
            }
        }
        let status = if failed > 0 {
            CiStatus::Failure
        } else if pending > 0 {
            CiStatus::Pending
        } else if passed > 0 {
            CiStatus::Success
        } else {
            CiStatus::Unknown
        };
        out.insert(
            *oid,
            CheckSummary {
                oid: *oid,
                status,
                passed,
                failed,
                pending,
                details_url,
            },
        );
    }
    Ok(out)
}

fn classify_gitlab_state(state: &str) -> CiStatus {
    match state {
        "success" => CiStatus::Success,
        "failed" => CiStatus::Failure,
        // GitLab enumerates a handful of "work-in-progress" states;
        // lumping them all into Pending keeps the badge legible
        // without losing information (the tooltip count still shows
        // the raw pending total).
        "running" | "pending" | "created" | "waiting_for_resource" | "preparing"
        | "scheduled" => CiStatus::Pending,
        "canceled" | "skipped" | "manual" => CiStatus::Neutral,
        _ => CiStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_state_classifier() {
        assert_eq!(classify_github_state("success"), CiStatus::Success);
        assert_eq!(classify_github_state("failure"), CiStatus::Failure);
        assert_eq!(classify_github_state("error"), CiStatus::Failure);
        assert_eq!(classify_github_state("pending"), CiStatus::Pending);
        assert_eq!(classify_github_state("weird"), CiStatus::Unknown);
    }

    #[test]
    fn gitlab_state_classifier() {
        assert_eq!(classify_gitlab_state("success"), CiStatus::Success);
        assert_eq!(classify_gitlab_state("failed"), CiStatus::Failure);
        assert_eq!(classify_gitlab_state("pending"), CiStatus::Pending);
        assert_eq!(classify_gitlab_state("running"), CiStatus::Pending);
        assert_eq!(classify_gitlab_state("canceled"), CiStatus::Neutral);
    }

    #[test]
    fn check_run_running_is_pending_regardless_of_conclusion() {
        let r = GhCheckRun {
            status: "in_progress".into(),
            conclusion: Some("success".into()),
            html_url: None,
        };
        assert_eq!(classify_check_run(&r), CiStatus::Pending);
    }

    #[test]
    fn check_run_completed_success() {
        let r = GhCheckRun {
            status: "completed".into(),
            conclusion: Some("success".into()),
            html_url: None,
        };
        assert_eq!(classify_check_run(&r), CiStatus::Success);
    }
}
