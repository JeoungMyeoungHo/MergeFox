//! Probe configured provider accounts against a private HTTPS git URL to
//! find one that authenticates, so the subsequent `git clone` runs as
//! that account without forcing the user to pick up front.
//!
//! Strategy
//! --------
//! 1. Only HTTPS (or HTTP) URLs are probed. SSH clones route through
//!    ssh-agent / `~/.ssh/config` and we can't substitute a key
//!    programmatically without confusing multi-key setups — those keep
//!    the existing "whatever the system picks" behaviour.
//! 2. Candidate accounts are those whose `ProviderKind` matches the URL
//!    host (github.com → GitHub accounts, etc.). The last-winning
//!    account for that host is tried first so the common case is a
//!    single `ls-remote` call instead of N.
//! 3. For each candidate we run `git ls-remote <url-with-pat> HEAD` with
//!    `GIT_TERMINAL_PROMPT=0` so a missing / invalid token fails fast
//!    instead of hanging on a hidden credential prompt.
//! 4. The first exit-0 wins. Caller receives `(AccountId, authed_url)`;
//!    `authed_url` embeds the PAT in the userinfo component for a
//!    one-shot `git clone`. Caller MUST rewrite `origin` to the clean
//!    URL after cloning — a URL with a token in `.git/config` is a
//!    credential leak into every repo backup, diff, and screenshot.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use secrecy::{ExposeSecret, SecretString};

use crate::providers::{AccountId, ProviderAccount, ProviderKind};

/// `host` → last account that successfully authenticated against it.
/// Tried first on subsequent probes; dropped on process exit.
static WINNER_CACHE: OnceLock<Mutex<HashMap<String, AccountId>>> = OnceLock::new();

fn winner_cache() -> &'static Mutex<HashMap<String, AccountId>> {
    WINNER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone)]
pub struct AuthedClone {
    pub account: AccountId,
    /// URL with credentials injected. Never store this in git config.
    pub authed_url: String,
}

/// Try each candidate account against `url` via `git ls-remote`.
///
/// Returns `Some` on the first authenticated success. `None` means
/// either the URL isn't a candidate for probing (SSH, unparseable, no
/// matching accounts) or every candidate's token was missing / rejected.
/// In either case the caller should fall through to whatever the
/// system `git` would have done natively.
pub fn probe(url: &str, accounts: &[ProviderAccount]) -> Option<AuthedClone> {
    let parsed = crate::git_url::parse(url)?;
    if !(parsed.canonical.starts_with("https://") || parsed.canonical.starts_with("http://")) {
        return None;
    }

    let candidates = order_candidates(&parsed.host, accounts);
    if candidates.is_empty() {
        return None;
    }

    for account in candidates {
        let Ok(Some(token)) = crate::providers::pat::load_pat(&account.id) else {
            continue;
        };
        let authed_url = embed_token(&parsed, &account.id, &token);
        if ls_remote_ok(&authed_url) {
            remember_winner(&parsed.host, &account.id);
            return Some(AuthedClone {
                account: account.id.clone(),
                authed_url,
            });
        }
    }
    None
}

fn order_candidates<'a>(host: &str, accounts: &'a [ProviderAccount]) -> Vec<&'a ProviderAccount> {
    let winner = winner_cache()
        .lock()
        .ok()
        .and_then(|m| m.get(host).cloned());
    let mut ordered: Vec<&ProviderAccount> = accounts
        .iter()
        .filter(|a| host_matches(&a.id.kind, host))
        .collect();
    // Stable sort with winner-first key keeps the rest in their saved
    // order, which matches what the user sees in Settings.
    ordered.sort_by_key(|a| match &winner {
        Some(w) if &a.id == w => 0,
        _ => 1,
    });
    ordered
}

fn remember_winner(host: &str, id: &AccountId) {
    if let Ok(mut m) = winner_cache().lock() {
        m.insert(host.to_string(), id.clone());
    }
}

fn host_matches(kind: &ProviderKind, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    match kind {
        ProviderKind::GitHub => host == "github.com",
        ProviderKind::GitLab => host == "gitlab.com",
        ProviderKind::Bitbucket => host == "bitbucket.org",
        ProviderKind::AzureDevOps => {
            host.ends_with("dev.azure.com") || host.ends_with("visualstudio.com")
        }
        ProviderKind::Codeberg => host == "codeberg.org",
        ProviderKind::Gitea { instance } => instance
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .map(|h| h.eq_ignore_ascii_case(&host))
            .unwrap_or(false),
        ProviderKind::Generic { host: h } => h.eq_ignore_ascii_case(&host),
    }
}

/// Build an HTTPS URL with the PAT embedded in the userinfo component.
///
/// GitHub accepts `https://x-access-token:<pat>@github.com/…` as a
/// documented token format; other providers use `<username>:<pat>`,
/// which the `git-credential` default also expects.
fn embed_token(parsed: &crate::git_url::GitUrl, id: &AccountId, token: &SecretString) -> String {
    let user = match id.kind {
        ProviderKind::GitHub => "x-access-token",
        _ => id.username.as_str(),
    };
    format!(
        "https://{}:{}@{}/{}/{}.git",
        userinfo_encode(user),
        userinfo_encode(token.expose_secret()),
        parsed.host,
        parsed.owner,
        parsed.repo
    )
}

/// Percent-encode characters that would break the userinfo component
/// of an HTTPS URL. PATs routinely contain `/`, `+`, `=`, and sometimes
/// `:` / `@`, any of which would cause git to mis-parse the URL.
fn userinfo_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Run `git ls-remote <url> HEAD` with a 10s ceiling. Returns true on
/// exit code 0. Credential helpers are disabled (`-c credential.helper=`)
/// so a stale cached credential from a previous session can't falsely
/// mark an account as authenticated.
fn ls_remote_ok(authed_url: &str) -> bool {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "credential.helper=",
        "ls-remote",
        "--exit-code",
        authed_url,
        "HEAD",
    ]);
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_ASKPASS", "/bin/echo");

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_encode_escapes_tricky_bytes() {
        assert_eq!(userinfo_encode("a/b+c=d"), "a%2Fb%2Bc%3Dd");
        assert_eq!(userinfo_encode("plain-Token.1_2~3"), "plain-Token.1_2~3");
    }

    #[test]
    fn host_matches_known_providers() {
        assert!(host_matches(&ProviderKind::GitHub, "github.com"));
        assert!(!host_matches(&ProviderKind::GitHub, "gitlab.com"));
        assert!(host_matches(&ProviderKind::GitLab, "GitLab.com"));
        assert!(host_matches(
            &ProviderKind::Gitea {
                instance: "https://git.example.com".into()
            },
            "git.example.com"
        ));
    }
}
