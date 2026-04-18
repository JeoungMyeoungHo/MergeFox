//! Task 1 — generate a Conventional Commits message from a staged diff.
//!
//! On grammar-capable endpoints we force the Conventional Commit shape
//! at decode time (`GRAMMAR_COMMIT_MSG`) so even a 0.5B model can't
//! emit prose. On everything else we lean on a strong system prompt
//! plus a regex-based fallback parser — if the model still produces
//! "Sure! Here is your message: feat: ...", the regex finds the good
//! part and drops the preamble.

use crate::ai::change_signals;
use crate::ai::client::{AiClient, CompletionRequest, Msg, Role};
use crate::ai::config::Protocol;
use crate::ai::diff_summarizer::summarize_for_prompt;
use crate::ai::error::{AiError, Result};
use crate::ai::grammars::GRAMMAR_COMMIT_MSG;

#[derive(Debug, Clone, Default)]
pub struct CommitMessageOpts {
    /// Cap on prompt tokens dedicated to the diff. Remaining budget
    /// goes to the system prompt and the model's output. When zero
    /// (the default) we auto-size from `context_window_tokens`.
    pub diff_budget_tokens: u32,
    /// Endpoint context window (tokens). Used to auto-size the diff
    /// budget so a 4K local model doesn't get a 3000-token diff plus
    /// overhead and blow past its limit. Zero means "don't know" —
    /// we fall back to a conservative 1200 in that case.
    pub context_window_tokens: u32,
    /// Hint for the model about the scope (e.g. "ui", "git") — empty
    /// means "let the model infer from paths".
    pub scope_hint: Option<String>,
    /// When true, ask for a body paragraph; otherwise title-only.
    pub include_body: bool,
    /// Project conventions learned from recent `git log`. `None` means
    /// "don't inject a PROJECT CONVENTIONS block" — fine for first
    /// commits or callers that can't see the repo path.
    pub conventions: Option<crate::ai::repo_conventions::RepoConventions>,
    /// When true, the task first asks the model to write 1–3 plain-
    /// English intent bullets about what the diff accomplishes, then
    /// feeds those back into a second call that emits the Conventional
    /// Commits header. Smaller models land on better scopes this way
    /// because they don't have to format + reason about intent at the
    /// same time. Costs one extra round-trip.
    pub two_phase: bool,
}

#[derive(Debug, Clone)]
pub struct CommitSuggestion {
    pub title: String,
    pub body: Option<String>,
    pub commit_type: String,
    pub scope: Option<String>,
    /// Forwarded from the diff analyser — if present, the UI should
    /// offer a "split into multiple commits" action (hand the diff
    /// to `commit_composer`) rather than blindly taking the single
    /// message we just produced.
    pub segmentation_advice: Option<crate::ai::change_signals::SegmentationAdvice>,
}

const CONVENTIONAL_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// Max parse retries. One initial attempt plus up to N-1 regenerations
/// with feedback; picked at 3 because endpoint-side parse failures are
/// almost always fixed by attempt 2, and a 3rd bite is cheap insurance
/// against rare flakes without meaningfully inflating latency.
const MAX_PARSE_RETRIES: u8 = 3;

/// Conventional Commits recommends subjects under 72 chars (50 is the
/// stricter old-school cap). Endpoints that ignore the GBNF grammar
/// — notably OpenAI-hosted and LM Studio — regularly exceed it despite
/// the prompt instruction. We truncate at parse time instead of
/// re-prompting because a single extra API round-trip isn't worth 20
/// characters of subject and the truncated version is usually fine
/// (models over-qualify; the first 72 chars carry the intent).
const MAX_SUBJECT_CHARS: usize = 72;

/// Forgiveness margin before we trigger a retry for "too long". A
/// subject that's 73–80 chars is close enough that hard-truncating is
/// nicer than another round-trip; past this bound we regenerate.
const OVERLENGTH_TOLERANCE: usize = 8;

/// Rough token estimate for a string — 4 chars ≈ 1 token on average
/// for English + code mix, matching the byte/token heuristic used by
/// `diff_summarizer`. We use this for preflight context checks only;
/// callers that need precise counts should ask the endpoint.
fn estimate_tokens(s: &str) -> u32 {
    if s.is_empty() {
        0
    } else {
        ((s.len() / 4).max(1)) as u32
    }
}

/// Tokens consumed by everything in the prompt that doesn't depend on
/// the diff summary — conventions block, signals block, per-file
/// block, segmentation advice, plus a baked-in estimate for the
/// fixed system prompt template. Intent block is excluded because
/// it's produced mid-run; we bake a reserve for it into the caller's
/// `SAFETY_MARGIN`.
fn estimate_prompt_fixed_cost(
    conventions_block: &str,
    signals_block: &str,
    per_file_block: &str,
    segmentation_block: &str,
) -> u32 {
    // System prompt is ~450 English chars → ~110 tokens.
    const SYSTEM_BASE_TOKENS: u32 = 120;
    SYSTEM_BASE_TOKENS
        + estimate_tokens(conventions_block)
        + estimate_tokens(signals_block)
        + estimate_tokens(per_file_block)
        + estimate_tokens(segmentation_block)
}

/// Trim `s` to at most `max` characters, attempting to cut at the
/// nearest word boundary so we don't leave a subject ending mid-word.
fn truncate_subject(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Walk to the byte position for the max-th char, then back up to
    // the last ASCII whitespace if we're in the middle of a word.
    let truncated: String = s.chars().take(max).collect();
    match truncated.rfind(char::is_whitespace) {
        Some(idx) if idx > max / 2 => truncated[..idx].trim_end().to_string(),
        _ => truncated.trim_end().to_string(),
    }
}

/// Public entry point. `diff` is the full staged diff — we trim it
/// before prompting.
///
/// Retry policy: if the model returns something we can't parse
/// (multi-scope, preamble survives `strip_code_fence`, wrong type,
/// …) we retry up to `MAX_PARSE_RETRIES` times, feeding the previous
/// offending output back as a "this failed validation, try again"
/// line. Retries only cover `Parse` errors — network / auth / rate
/// limits bubble up immediately so the UI can surface the real cause.
pub async fn gen_commit_message(
    client: &dyn AiClient,
    diff: &str,
    opts: CommitMessageOpts,
) -> Result<CommitSuggestion> {
    if diff.trim().is_empty() {
        return Err(AiError::Parse {
            parser: "commit_message: empty diff".into(),
            raw: String::new(),
        });
    }

    // Diff budget: reserve ~1500 tokens of the endpoint's context for
    // system prompt + signals block + conventions + intent bullets +
    // output + safety margin. Whatever's left goes to the diff. Caps
    // at 3000 because past that the per-file fair-share is plenty and
    // bigger diffs just raise latency without helping classification.
    //
    // Explicit caller override wins; a zero `context_window_tokens`
    // means "no endpoint info" so we default to 1200 (matches the
    // pre-autosize behaviour).
    let budget = if opts.diff_budget_tokens != 0 {
        opts.diff_budget_tokens
    } else if opts.context_window_tokens > 0 {
        // Raised from 1500 after adding per-file + segmentation blocks
        // to the user message; on a 4K model those plus the intent +
        // retry-history can easily hit 2K tokens before the diff even
        // starts.
        const RESERVED_OVERHEAD: u32 = 2600;
        opts.context_window_tokens
            .saturating_sub(RESERVED_OVERHEAD)
            .min(3000)
            .max(600)
    } else {
        1200
    };
    let trimmed = summarize_for_prompt(diff, budget);

    // Compute the change signals from the FULL diff, not the trimmed
    // one — otherwise the "dominant module" / "new public API" picture
    // is biased by whatever the summariser decided to keep. Signals
    // are tiny (tens of bytes once rendered) so paying full-diff cost
    // here is negligible.
    let signals = change_signals::analyze(diff);
    let signals_block = signals.render_for_prompt();
    let per_file_block = signals.render_per_file_changes();
    let segmentation_block = signals.render_segmentation_tail();

    // Project conventions block — empty string if caller didn't pass
    // `opts.conventions` or if sample size is below the reliability
    // threshold. Safe to concatenate unconditionally.
    let conventions_block = opts
        .conventions
        .as_ref()
        .map(|c| c.render_for_prompt())
        .unwrap_or_default();

    // Preflight token estimate. If we can see we'll overflow BEFORE
    // calling the endpoint, surface a ContextOverflow error with a
    // concrete "needs at least N tokens" number so the UI can point
    // the user at Settings → AI → Context window. Trying to call
    // anyway wastes a round-trip and yields an opaque 400 from the
    // backend.
    if opts.context_window_tokens > 0 {
        let fixed_cost = estimate_prompt_fixed_cost(
            &conventions_block,
            &signals_block,
            &per_file_block,
            &segmentation_block,
        );
        // Conservative output reserve: the body phase can take ~260
        // tokens and we want retries not to re-trip this check.
        const OUTPUT_RESERVE: u32 = 400;
        const SAFETY_MARGIN: u32 = 200;
        let required = fixed_cost + budget + OUTPUT_RESERVE + SAFETY_MARGIN;
        if required > opts.context_window_tokens {
            return Err(AiError::ContextOverflow {
                used: required,
                budget: opts.context_window_tokens,
            });
        }
    }

    // If the caller didn't hint at a scope, derive one. Preference:
    //   1) explicit caller hint,
    //   2) a repo scope that matches the signals-derived module,
    //   3) the signals-derived module itself as a fallback.
    // Step 2 lets repo conventions act as a spell-checker on paths
    // — if the diff is dominated by `src/ui/*` but the repo never uses
    // `ui` as a scope (uses `gui` instead), we borrow `gui`.
    let effective_scope_hint = resolve_scope_hint(&opts, &signals);
    let scope_line = match &effective_scope_hint {
        Some(s) if !s.is_empty() => format!("\nPreferred scope: {}", s),
        _ => String::new(),
    };

    // Body default: ON unless the repo clearly avoids bodies. Bodies
    // are where Conventional Commits are MEANT to live — cramming
    // every detail into a 72-char subject is a bad tradeoff. We flip
    // off only when convention signal is reliable AND body rate is
    // very low (≤15%); at that point the project's norm is terse
    // header-only commits and we honour it.
    let include_body = if opts.include_body {
        true
    } else if let Some(c) = opts.conventions.as_ref() {
        if c.is_reliable() {
            c.body_rate > 0.15
        } else {
            true
        }
    } else {
        true
    };

    // Phase 2 ALWAYS writes just the header — body generation moved
    // to its own Phase 2b call below. Separating the two lets small
    // models focus on each task independently; otherwise they either
    // skip the body entirely ("just write the header, that's enough")
    // or cram the body material into a 90-char subject.
    let body_clause =
        "Output EXACTLY one line: the header only. No blank line, no body, no code fences.";

    // Hard constraints derived from the signals, expressed as
    // prohibitions the model can't misread as "suggestions". The
    // order matches the signal block above so the model can cross-
    // reference the two.
    let mut bans = Vec::new();
    if !signals.docs_only {
        bans.push("`docs`");
    }
    if !signals.tests_only {
        bans.push("`test`");
    }
    let ban_clause = if bans.is_empty() {
        String::new()
    } else {
        format!(
            " Given the CLASSIFICATION SIGNALS below, do NOT use type {} — those are only valid \
             when the corresponding signal is YES.",
            bans.join(" or ")
        )
    };

    let system = format!(
        "You are a Git commit message writer. Respond with exactly one Conventional Commits \
         message and nothing else. Format: `<type>(<scope>)?: <subject>` where <type> is one of \
         feat, fix, docs, style, refactor, perf, test, build, ci, chore, revert. \
         The scope is OPTIONAL and, when present, MUST be a single lowercase token made of \
         letters, digits, dashes, or underscores only — never commas, slashes, spaces, or \
         multiple scopes. Pick the single most specific scope rather than listing several. \
         Subject must be imperative mood (\"add X\", not \"added X\" or \"adds X\") and under 72 \
         characters. Do not wrap the output in quotes, code fences, or any preamble. \
         Use the CLASSIFICATION SIGNALS below to choose the type — they are deterministic facts \
         about the diff. Do NOT infer type from the relative volume of lines or comments.{}{}{}",
        ban_clause, body_clause, scope_line
    );

    // Optional Phase-1 intent extraction. Runs before the Conventional
    // Commits pass so the model can concentrate on "what does this
    // change accomplish" without also formatting. When it fails we
    // fall back to the single-phase flow — we never surface a Phase-1
    // error to the user, since the phase is an optimisation.
    let intent_block = if opts.two_phase {
        match extract_intent_bullets(
            client,
            &signals_block,
            &conventions_block,
            &segmentation_block,
            &per_file_block,
            &trimmed,
        )
        .await
        {
            Ok(bullets) if !bullets.trim().is_empty() => {
                format!("INTENT (plain-English goals of this diff):\n{bullets}\n")
            }
            _ => String::new(),
        }
    } else {
        String::new()
    };

    let base_user =
        format!("{conventions_block}{signals_block}{segmentation_block}{per_file_block}{intent_block}\nStaged diff:\n\n{trimmed}");

    let mut last_raw: Option<String> = None;
    let mut last_parse_err: Option<AiError> = None;
    for attempt in 0..MAX_PARSE_RETRIES {
        let user = match &last_raw {
            None => base_user.clone(),
            Some(prev) => format!(
                "{base_user}\n\nYour previous reply was:\n{prev}\n\n\
                 That reply did not parse as a valid Conventional Commits header. \
                 The scope, if any, must be a SINGLE lowercase token (letters, digits, \
                 dash, underscore). Reply with ONLY the corrected commit message."
            ),
        };

        // Grammar only fires on endpoints that advertise support — the
        // client drops it otherwise. Supplying it unconditionally here
        // keeps task code simple.
        let req = CompletionRequest {
            system: system.clone(),
            messages: vec![Msg {
                role: Role::User,
                content: user,
            }],
            // Header-only: ~45 tokens is just enough for
            // `feat(scope): ` plus a full 72-char subject. Body (when
            // wanted) is generated in a separate call below.
            max_tokens: 45,
            // Bump temperature slightly on retry so the model doesn't
            // deterministically reproduce the same malformed output.
            temperature: 0.2 + 0.1 * f32::from(attempt),
            grammar: Some(GRAMMAR_COMMIT_MSG.to_string()),
            json_schema: None,
            stop: vec![],
        };

        debug_log_prompt("phase2", &req.system, &req.messages);
        let resp = client.complete(req).await?;
        debug_log_response("phase2", &resp.text);
        match parse_commit_message(&resp.text) {
            Ok(mut sugg) => {
                // Phase 3 (conditional): compress over-length subject.
                // Small models routinely ignore the 72-char cap even
                // under grammar + explicit instructions; rather than
                // burning further Phase-2 retries on the same output,
                // we let a focused "shorten this" call rewrite just
                // the subject. Costs one extra round-trip but only
                // when actually needed.
                if needs_shrink(&sugg) {
                    if let Some(shortened) = shrink_subject(client, &sugg).await {
                        apply_new_subject(&mut sugg, &shortened);
                    } else {
                        hard_truncate_subject(&mut sugg);
                    }
                }

                // Phase 2b: generate the body. Separate from Phase 2
                // so the header-stage prompt can enforce a strict
                // one-line output without confusing the model about
                // when to add the body paragraph. Cheap small models
                // routinely ignore a "write a body paragraph after"
                // clause when the main goal is "produce the header";
                // giving the body its own focused call fixes this.
                if include_body && sugg.body.is_none() {
                    if let Some(body) = generate_body(
                        client,
                        &sugg.title,
                        &signals_block,
                        &conventions_block,
                        &segmentation_block,
                        &per_file_block,
                        &intent_block,
                        &trimmed,
                    )
                    .await
                    {
                        sugg.body = Some(body);
                    }
                }

                sugg.segmentation_advice = signals.segmentation_advice.clone();
                return Ok(sugg);
            }
            Err(AiError::Parse { raw, parser }) => {
                last_raw = Some(raw.clone());
                last_parse_err = Some(AiError::Parse { raw, parser });
                continue;
            }
            Err(other) => return Err(other),
        }
    }
    // All retries exhausted — surface the last parse failure verbatim
    // so the UI can still show the raw output for manual salvage.
    Err(last_parse_err.expect("loop exits only via return or Parse error"))
}

/// True when the parsed subject is long enough that we want to spend
/// a round-trip compressing it. Below this bound hard truncation is
/// nicer than an extra API call.
fn needs_shrink(sugg: &CommitSuggestion) -> bool {
    let subject_chars = sugg
        .title
        .rsplit(':')
        .next()
        .unwrap_or(&sugg.title)
        .trim_start()
        .chars()
        .count();
    subject_chars > MAX_SUBJECT_CHARS + OVERLENGTH_TOLERANCE
}

/// Phase 3: ask the model to rewrite just the subject under the length
/// cap, preserving meaning. Returns the new subject on success or
/// `None` if the model's reply can't be used (empty, still too long,
/// injected formatting, etc.) — the caller falls back to truncation.
async fn shrink_subject(client: &dyn AiClient, sugg: &CommitSuggestion) -> Option<String> {
    let current_subject = sugg
        .title
        .rsplit(':')
        .next()
        .unwrap_or(&sugg.title)
        .trim_start()
        .to_string();

    let system = format!(
        "Rewrite the given git commit subject so it fits in {MAX_SUBJECT_CHARS} characters. \
         Preserve the core intent; drop qualifiers, trailing clauses, and secondary details. \
         Reply with ONLY the rewritten subject — no prefix, no quotes, no code fences, no \
         explanation. Imperative mood (\"add X\", not \"added X\"). Single line."
    );
    let user = format!(
        "Current subject ({} chars):\n{current_subject}",
        current_subject.chars().count()
    );

    let req = CompletionRequest {
        system,
        messages: vec![Msg {
            role: Role::User,
            content: user,
        }],
        // Tight cap — any longer than ~25 tokens is a sign the model
        // ignored us and is writing prose.
        max_tokens: 25,
        temperature: 0.2,
        grammar: None,
        json_schema: None,
        stop: vec!["\n".into()],
    };

    debug_log_prompt("phase3-shrink", &req.system, &req.messages);
    let resp = client.complete(req).await.ok()?;
    debug_log_response("phase3-shrink", &resp.text);

    let candidate = resp.text.trim().trim_matches('"').trim_matches('`').trim();
    // Strip a type-prefix if the model re-added one ("feat: add x").
    let candidate = candidate
        .splitn(2, ':')
        .nth(1)
        .map(str::trim)
        .unwrap_or(candidate);

    if candidate.is_empty() {
        return None;
    }
    if candidate.chars().count() > MAX_SUBJECT_CHARS + OVERLENGTH_TOLERANCE {
        return None;
    }
    Some(candidate.to_string())
}

fn apply_new_subject(sugg: &mut CommitSuggestion, new_subject: &str) {
    sugg.title = match &sugg.scope {
        Some(scope) => format!("{}({}): {}", sugg.commit_type, scope, new_subject),
        None => format!("{}: {}", sugg.commit_type, new_subject),
    };
}

/// Phase 2b: given the already-parsed header, ask the model for a body
/// paragraph. Separating this from Phase 2 fixes two failure modes we
/// saw on small (2B) models:
///
///   1. Asked for "header + optional body", they produce only a
///      header and call it done, even when the project convention
///      strongly suggests bodies.
///   2. Asked for "header + body", they cram body-material into the
///      header and blow past 72 characters.
///
/// With a dedicated call the model is told "the header is decided;
/// explain the WHY below it" and treats the body as the primary task.
/// Grammar is disabled here — free prose, soft cap via max_tokens.
async fn generate_body(
    client: &dyn AiClient,
    header: &str,
    signals_block: &str,
    conventions_block: &str,
    segmentation_block: &str,
    per_file_block: &str,
    intent_block: &str,
    trimmed_diff: &str,
) -> Option<String> {
    let system = "You write the body of a git commit message. The header is already decided; \
                  your job is to explain WHAT changed and WHY, using specific names from the \
                  signals.\n\
                  Rules:\n\
                  (1) Reference at least 2 specific symbols or files from NEW PUBLIC SYMBOLS / \
                  PER-FILE CHANGES by name (e.g. `Phase 2b body generation`, `SegmentationAdvice`, \
                  `diagnose_load`).\n\
                  (2) If the CLASSIFICATION SIGNALS show multiple distinct concerns, cover each \
                  briefly — one sentence per concern. Do not collapse them.\n\
                  (3) 2-8 lines, each wrapped at ~72 characters. Plain prose (or a short \
                  bulleted list when the change is itself a list of independent items).\n\
                  (4) FORBIDDEN: vague verbs such as 'improve', 'refactor', 'enhance', 'update', \
                  'clean up', 'polish', 'various changes' used without a named symbol. Also \
                  forbidden: repeating the header's subject verbatim, restating the Conventional \
                  Commits type/scope, markdown headings, code fences.\n\
                  Reply with ONLY the body text — no prefix, no surrounding blank lines, no quotes.";

    let user = format!(
        "{conventions_block}{signals_block}{segmentation_block}{per_file_block}{intent_block}\n\
         Header (already decided):\n{header}\n\n\
         Diff:\n\n{trimmed_diff}\n\n\
         Write the body now. Cover every distinct concern from the signals, reference \
         specific symbol names from NEW PUBLIC SYMBOLS / PER-FILE CHANGES. Avoid vague \
         verbs. 2-8 lines, ~72 chars each."
    );

    let req = CompletionRequest {
        system: system.to_string(),
        messages: vec![Msg {
            role: Role::User,
            content: user,
        }],
        max_tokens: 260,
        temperature: 0.25,
        grammar: None,
        json_schema: None,
        stop: vec![],
    };

    debug_log_prompt("phase2b-body", &req.system, &req.messages);
    let resp = client.complete(req).await.ok()?;
    debug_log_response("phase2b-body", &resp.text);

    let cleaned = strip_code_fence(resp.text.trim()).trim();
    if cleaned.is_empty() {
        return None;
    }
    // Defensively drop a re-emitted header line so we don't end up
    // with "feat(x): y\n\nfeat(x): y\n\nreal body…" after concat.
    let body: String = cleaned
        .lines()
        .filter(|line| {
            let t = line.trim();
            t != header && !looks_like_conventional_header(t)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim().to_string();
    if body.is_empty() {
        return None;
    }
    Some(body)
}

fn looks_like_conventional_header(line: &str) -> bool {
    // Any line starting with `<type>:` or `<type>(...):` where type
    // is in the Conventional Commits set is almost certainly a stray
    // header rather than real body content.
    let colon = match line.find(':') {
        Some(i) => i,
        None => return false,
    };
    let prefix = line[..colon].trim();
    let ty = prefix
        .split('(')
        .next()
        .unwrap_or(prefix)
        .trim_end_matches('!');
    CONVENTIONAL_TYPES.contains(&ty)
}

fn hard_truncate_subject(sugg: &mut CommitSuggestion) {
    let subject_raw = sugg
        .title
        .rsplit(':')
        .next()
        .unwrap_or(&sugg.title)
        .trim_start()
        .to_string();
    let truncated = truncate_subject(&subject_raw, MAX_SUBJECT_CHARS);
    apply_new_subject(sugg, &truncated);
}

/// Pick a scope hint for the prompt. Prefers the caller's explicit
/// hint, then an intersection of diff signals with learned repo
/// conventions, and finally the raw module tag from the diff.
fn resolve_scope_hint(
    opts: &CommitMessageOpts,
    signals: &change_signals::ChangeSignals,
) -> Option<String> {
    if let Some(explicit) = opts.scope_hint.as_deref() {
        if !explicit.is_empty() {
            return Some(explicit.to_string());
        }
    }

    let module = signals.dominant_module.as_deref()?;

    // If the module name also appears in the repo's active scope
    // vocabulary, prefer the exact casing/variant the project uses.
    if let Some(conv) = opts.conventions.as_ref() {
        if conv.is_reliable() {
            if let Some(matched) = conv
                .common_scopes
                .iter()
                .find(|s| s.scope.eq_ignore_ascii_case(module))
            {
                return Some(matched.scope.clone());
            }
        }
    }

    Some(module.to_string())
}

/// Phase-1 of the two-phase flow: ask the model for 1–3 plain-English
/// bullets describing the intent behind the diff. No Conventional
/// Commits format, no type / scope discipline — just content. Bullets
/// feed into Phase 2 as a stable summary the second call can lean on
/// without re-reading the full diff.
async fn extract_intent_bullets(
    client: &dyn AiClient,
    signals_block: &str,
    conventions_block: &str,
    segmentation_block: &str,
    per_file_block: &str,
    trimmed_diff: &str,
) -> Result<String> {
    let system = "You summarise git diffs as a bulleted list of DISTINCT concerns. \
                  Rules: \
                  (1) One bullet per distinct concern — never merge two concerns into one bullet. \
                  (2) Each bullet MUST name a specific symbol, function, file, or pipeline stage \
                  from the PER-FILE CHANGES / NEW PUBLIC SYMBOLS sections. \
                  (3) Start each bullet with a strong imperative verb (`- Add X`, `- Tighten Y`, \
                  `- Extract Z`) followed by the named object. \
                  (4) FORBIDDEN as the sole content of a bullet: generic verbs like 'improve', \
                  'refactor', 'enhance', 'update', 'clean up', 'polish', 'various changes' \
                  when not followed by a concrete named subject. \
                  (5) Produce 3 to 6 bullets for multi-file diffs; 1-2 suffice only for a \
                  single-file change. \
                  (6) Do not use Conventional Commits format, types, scopes, or code fences. \
                  Lead each bullet with `- `.";

    let user = format!(
        "{conventions_block}{signals_block}{segmentation_block}{per_file_block}\n\
         Using the signals above, list EVERY distinct concern this diff addresses — one \
         bullet per concern, reference specific symbols from NEW PUBLIC SYMBOLS / \
         PER-FILE CHANGES. Then STOP.\n\nDiff:\n\n{trimmed_diff}"
    );

    let req = CompletionRequest {
        system: system.to_string(),
        messages: vec![Msg {
            role: Role::User,
            content: user,
        }],
        // Room for up to ~6 bullets of concrete prose. Past this the
        // model starts inventing concerns rather than summarising.
        max_tokens: 400,
        temperature: 0.2,
        // Intent extraction is free-form — grammars would fight us.
        grammar: None,
        json_schema: None,
        stop: vec![],
    };

    debug_log_prompt("phase1", &req.system, &req.messages);
    let resp = client.complete(req).await?;
    debug_log_response("phase1", &resp.text);
    Ok(tidy_bullets(&resp.text))
}

/// Dev-only instrumentation: when `MERGEFOX_LOG_AI_PROMPT=1`, dump the
/// prompt and response to stderr so an out-of-band probe can diff the
/// harness's output against the model's behaviour on real repos without
/// wiring a full test fixture.
fn debug_log_prompt(tag: &str, system: &str, messages: &[Msg]) {
    if std::env::var("MERGEFOX_LOG_AI_PROMPT").ok().as_deref() != Some("1") {
        return;
    }
    eprintln!("\n==== [{tag}] SYSTEM ====\n{system}");
    for (i, m) in messages.iter().enumerate() {
        eprintln!("\n==== [{tag}] MSG {i} ({:?}) ====\n{}", m.role, m.content);
    }
}

fn debug_log_response(tag: &str, text: &str) {
    if std::env::var("MERGEFOX_LOG_AI_PROMPT").ok().as_deref() != Some("1") {
        return;
    }
    eprintln!("\n==== [{tag}] RESPONSE ====\n{text}");
}

/// Normalise the model's intent-phase output. Keeps only bullet-shaped
/// lines, caps to 3 bullets, drops any code fences the model added.
fn tidy_bullets(raw: &str) -> String {
    let stripped = strip_code_fence(raw.trim());
    let mut out = Vec::new();
    for line in stripped.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let content: String = if let Some(rest) = t.strip_prefix("- ") {
            rest.to_string()
        } else if let Some(rest) = t.strip_prefix("* ") {
            rest.to_string()
        } else if t
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            // "1. …" / "1) …" numbered output → drop the numeric prefix.
            let after_digits: String = t.chars().skip_while(|c| c.is_ascii_digit()).collect();
            after_digits
                .trim_start_matches(|c: char| c == '.' || c == ')' || c.is_whitespace())
                .to_string()
        } else {
            continue;
        };
        out.push(format!("- {}", content.trim()));
        // Up to 6 bullets — wide enough for a multi-concern commit
        // (which is why Phase 1's system prompt now explicitly asks
        // for "3 to 6 bullets for multi-file diffs").
        if out.len() >= 6 {
            break;
        }
    }
    out.join("\n")
}

/// Parse the model's reply into a structured suggestion.
///
/// We accept, in order of strictness:
///   1. exactly the grammar shape: `type(scope)?: subject\n\nbody?`;
///   2. the same, but wrapped in common LLM prose ("Here's...");
///   3. best-effort: strip code fences, scan for the first line that
///      starts with a known type.
pub(crate) fn parse_commit_message(text: &str) -> Result<CommitSuggestion> {
    // Strip trailing/leading whitespace and any surrounding code fence
    // a non-grammar model might have added.
    let cleaned = strip_code_fence(text.trim());

    // Scan lines for the first candidate header.
    for line in cleaned.lines() {
        let candidate = line.trim();
        if candidate.is_empty() {
            continue;
        }
        if let Some(parsed) = try_parse_header(candidate) {
            // Rebuild the title with the verbatim subject — length
            // enforcement happens as a post-processing phase in
            // `gen_commit_message`, not here, because fixing length
            // may require an extra model call and parse_commit_message
            // has no client access.
            let subject = candidate[candidate.find(':').expect("try_parse_header checked") + 1..]
                .trim_start()
                .to_string();
            let title = match &parsed.scope {
                Some(scope) => format!("{}({}): {}", parsed.commit_type, scope, subject),
                None => format!("{}: {}", parsed.commit_type, subject),
            };
            let body = extract_body_after(cleaned, candidate);
            return Ok(CommitSuggestion {
                title,
                body,
                commit_type: parsed.commit_type,
                scope: parsed.scope,
                // Populated by `gen_commit_message` after parse —
                // parsing alone has no diff access.
                segmentation_advice: None,
            });
        }
    }

    Err(AiError::Parse {
        parser: "commit_message: no conventional header found".into(),
        raw: cleaned.chars().take(512).collect(),
    })
}

fn strip_code_fence(s: &str) -> &str {
    // Hand-rolled because we don't want to pull in a regex crate. We
    // recognise ```<tag>?\n...\n``` at the start of the buffer.
    let s = s.trim();
    let stripped = s
        .strip_prefix("```")
        .and_then(|rest| {
            // drop optional language tag up to the first newline
            let nl = rest.find('\n')?;
            Some(&rest[nl + 1..])
        })
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim);
    stripped.unwrap_or(s)
}

struct ParsedHeader {
    commit_type: String,
    scope: Option<String>,
}

/// Parse `type(scope)?: subject`. Returns parsed header on success.
///
/// Scope tolerance: we accept common-mistake shapes like `(app, clone)`
/// or `(foo/bar)` at the raw level and normalize them — strip inner
/// whitespace, drop commas/slashes — before re-validating as a strict
/// single token. That way a model that ignored the "single scope only"
/// instruction still produces a parseable, canonical header instead of
/// forcing a retry loop.
fn try_parse_header(line: &str) -> Option<ParsedHeader> {
    // Must contain a colon.
    let colon = line.find(':')?;
    let prefix = &line[..colon];

    // Split off optional scope in parens.
    let (ty, scope) = if let Some(open) = prefix.find('(') {
        if !prefix.ends_with(')') {
            return None;
        }
        let ty = &prefix[..open];
        let raw_scope = &prefix[open + 1..prefix.len() - 1];
        if !raw_scope_recognisable(raw_scope) {
            return None;
        }
        let normalized = normalize_scope(raw_scope);
        if normalized.is_empty() || !scope_is_strict(&normalized) {
            return None;
        }
        (ty, Some(normalized))
    } else {
        (prefix, None)
    };

    if !CONVENTIONAL_TYPES.contains(&ty) {
        return None;
    }
    // Subject must be non-empty after `: `.
    let subject = line[colon + 1..].trim_start();
    if subject.is_empty() {
        return None;
    }
    Some(ParsedHeader {
        commit_type: ty.to_string(),
        scope,
    })
}

/// Characters we're willing to see inside a raw `(...)` scope at parse
/// time before normalisation. Wider than the strict allowlist because
/// real model output occasionally includes commas, slashes, or inner
/// whitespace that `normalize_scope` then collapses.
fn raw_scope_recognisable(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ',' | '/' | ' ' | '\t'))
}

/// Collapse a tolerated raw scope into the canonical single-token
/// form: drop whitespace, keep only the first comma/slash-delimited
/// segment (so `app, clone` → `app`, `foo/bar` → `foo`). We deliberately
/// pick "first segment" over "concatenate" because a single-word scope
/// reads cleaner in `git log` than `app-clone`.
fn normalize_scope(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    cleaned
        .split(|c| c == ',' || c == '/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Post-normalisation: final, strict validation against the set of
/// characters we persist into git.
fn scope_is_strict(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Find a body paragraph after the header line, if present.
fn extract_body_after(full: &str, header: &str) -> Option<String> {
    let idx = full.find(header)?;
    let after = &full[idx + header.len()..];
    // Expect at least one blank line before the body; otherwise the
    // following lines are probably a second header the model emitted
    // by mistake and we'd rather drop them.
    let after = after.trim_start_matches('\n');
    let after = after.trim_start_matches('\n');
    let body = after.trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

/// Convenience for callers on Anthropic endpoints: the GBNF grammar
/// is useless there, so they can build a request with the JSON-schema
/// path instead. Kept here (not in client) because it's prompt-scoped.
#[allow(dead_code)]
pub(crate) fn is_grammar_useful(protocol: Protocol) -> bool {
    matches!(protocol, Protocol::OpenAICompatible)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Live-model probe -------------------------------------------------
    //
    // Gated test that drives the full harness against a local LM Studio /
    // Ollama endpoint so we can iterate on prompt quality without
    // clicking the UI button every time. Off by default; enable with:
    //
    //   HARNESS_PROBE=1 \
    //   HARNESS_ENDPOINT=http://127.0.0.1:1234/v1 \
    //   HARNESS_MODEL=qwen3.5-2b \
    //   HARNESS_DIFF_FILE=/tmp/mergefox_diff.txt \
    //   HARNESS_REPO_PATH=/Users/me/code/mergefox \
    //   MERGEFOX_LOG_AI_PROMPT=1 \
    //   cargo test --bin mergefox probe_live_model -- --nocapture --ignored
    //
    // `--ignored` is the safety rail: the test never runs in normal
    // `cargo test` because without a live server it would hang.
    #[test]
    #[ignore]
    fn diagnose_user_config() {
        eprintln!("{}", crate::config::diagnose_load());
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn probe_live_model() {
        if std::env::var("HARNESS_PROBE").ok().as_deref() != Some("1") {
            eprintln!("HARNESS_PROBE not set; skipping.");
            return;
        }
        let endpoint_url =
            std::env::var("HARNESS_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:1234/v1".into());
        let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "qwen3.5-2b".into());
        let diff_file =
            std::env::var("HARNESS_DIFF_FILE").unwrap_or_else(|_| "/tmp/mergefox_diff.txt".into());
        let repo_path = std::env::var("HARNESS_REPO_PATH").ok();

        let diff = std::fs::read_to_string(&diff_file)
            .unwrap_or_else(|e| panic!("read diff file {diff_file}: {e}"));
        assert!(!diff.trim().is_empty(), "diff file must not be empty");

        let context_window: u32 = std::env::var("HARNESS_CONTEXT_WINDOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096);
        let endpoint = crate::ai::config::Endpoint {
            name: format!("probe:{model}"),
            protocol: Protocol::OpenAICompatible,
            base_url: endpoint_url,
            api_key: secrecy::SecretString::new(String::new()),
            model_id: model,
            context_window,
            max_output: 512,
            supports_grammar: false, // LM Studio usually ignores GBNF
            supports_streaming: false,
        };
        let client = crate::ai::client::build_client(endpoint);

        let conventions = repo_path
            .as_deref()
            .map(std::path::Path::new)
            .map(crate::ai::repo_conventions::load);

        let opts = CommitMessageOpts {
            conventions,
            two_phase: true,
            context_window_tokens: context_window,
            ..Default::default()
        };

        let result = gen_commit_message(client.as_ref(), &diff, opts).await;
        match result {
            Ok(s) => {
                eprintln!("\n==== PARSED SUGGESTION ====");
                eprintln!("title: {}", s.title);
                if let Some(b) = &s.body {
                    eprintln!("body:\n{b}");
                }
                if let Some(adv) = &s.segmentation_advice {
                    eprintln!("\n==== SEGMENTATION ADVICE ====");
                    eprintln!("reason: {}", adv.reason);
                    for g in &adv.groups {
                        eprintln!("  [{}] {:?}", g.label, g.paths);
                    }
                } else {
                    eprintln!("\n(no segmentation advice)");
                }
            }
            Err(e) => {
                eprintln!("\n==== ERROR ==== {e:?}");
                panic!("harness returned error");
            }
        }
    }

    #[test]
    fn parses_plain_header() {
        let r = parse_commit_message("feat(ui): add dark mode toggle").unwrap();
        assert_eq!(r.commit_type, "feat");
        assert_eq!(r.scope.as_deref(), Some("ui"));
        assert!(r.body.is_none());
    }

    #[test]
    fn parses_with_body() {
        let r = parse_commit_message("fix: handle empty diff\n\nReturn NotConfigured.").unwrap();
        assert_eq!(r.commit_type, "fix");
        assert_eq!(r.body.as_deref(), Some("Return NotConfigured."));
    }

    #[test]
    fn strips_code_fence_and_preamble() {
        let raw = "Sure, here:\n```\nrefactor: extract helper\n```";
        let r = parse_commit_message(raw).unwrap();
        assert_eq!(r.commit_type, "refactor");
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(parse_commit_message("wip: nothing").is_err());
    }

    #[test]
    fn normalizes_multi_scope_with_space_to_first_segment() {
        // Real failure seen in the wild — the AI emitted a comma-
        // separated scope. Parser takes the first segment, drops the
        // rest, and rebuilds a canonical title.
        let r = parse_commit_message("feat(app, clone): add upstream identity selection").unwrap();
        assert_eq!(r.commit_type, "feat");
        assert_eq!(r.scope.as_deref(), Some("app"));
        assert_eq!(r.title, "feat(app): add upstream identity selection");
    }

    #[test]
    fn normalizes_slashed_scope() {
        let r = parse_commit_message("fix(ui/commit): clear draft on dismiss").unwrap();
        assert_eq!(r.scope.as_deref(), Some("ui"));
        assert_eq!(r.title, "fix(ui): clear draft on dismiss");
    }

    #[test]
    fn rejects_scope_with_forbidden_chars_even_after_normalising() {
        // Special chars outside the tolerated set still fail fast —
        // we don't want to accept `<app>` by silently stripping angle
        // brackets, that's the kind of correction that hides bugs.
        assert!(parse_commit_message("feat(<app>): something").is_err());
    }

    #[test]
    fn normalize_scope_picks_first_segment() {
        assert_eq!(normalize_scope("app, clone"), "app");
        assert_eq!(normalize_scope("  foo "), "foo");
        assert_eq!(normalize_scope("a/b/c"), "a");
        assert_eq!(normalize_scope(""), "");
    }
}
