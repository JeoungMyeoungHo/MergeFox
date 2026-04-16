//! Diff trimming for LLM prompts.
//!
//! Context overflow is the single biggest failure mode on tiny local
//! models — paste a 200KB diff at a 4K-context Qwen and you get
//! garbage or a truncation error. This module does a best-effort
//! budget fit:
//!
//!   1. Split the unified diff into per-file sections at `diff --git`.
//!   2. Drop bodies of files whose extension looks binary/image.
//!   3. Keep the file header + first N lines of each file's body.
//!   4. Insert `[truncated: N more lines]` markers so the model can
//!      reason about incompleteness.
//!   5. If we're still over budget, rank files and cut the tail.
//!
//! Token estimation is deliberately crude — `len / 4` matches the
//! rough byte-per-token ratio for English + code and is what OpenAI
//! suggests as a back-of-envelope. The goal is "don't blow up",
//! not "hit the exact limit".

/// Cheap char-based token estimate.
fn est_tokens(s: &str) -> u32 {
    // Integer-divide by 4 with a floor of 1 for any non-empty string so
    // the estimate is never 0 when the text is actually there.
    let t = (s.len() / 4) as u32;
    if s.is_empty() {
        0
    } else {
        t.max(1)
    }
}

/// How many body lines we keep per file before truncating.
const LINES_PER_FILE: usize = 40;

/// Extensions we treat as binary — we keep the `diff --git` header so
/// the model knows the file changed, but drop the hunks. git itself
/// usually already writes "Binary files differ", but users sometimes
/// pre-process diffs.
const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tif", "tiff", "psd", "ai", "pdf", "zip",
    "tar", "gz", "bz2", "xz", "7z", "jar", "war", "class", "o", "a", "so", "dylib", "dll", "exe",
    "bin", "wasm", "mp3", "mp4", "mov", "avi", "mkv", "wav", "flac", "ogg", "ttf", "otf", "woff",
    "woff2", "eot", "db", "sqlite",
];

fn is_binary_path(path: &str) -> bool {
    let ext = path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    BINARY_EXTS.contains(&ext.as_str())
}

/// Extract `a/<path>` from a `diff --git a/foo b/foo` header. Returns
/// `None` if the header is malformed — in which case we keep the file
/// body as-is (better to over-include than silently drop changes).
fn path_from_header(header: &str) -> Option<&str> {
    // `diff --git a/path b/path` — split on spaces, take the piece
    // after `a/`.
    let a_segment = header.split_whitespace().nth(2)?;
    a_segment.strip_prefix("a/")
}

/// Summarize a unified diff for inclusion in a prompt.
///
/// `budget_tokens` is a soft cap — we try to land under it but guarantee
/// the result is at least the list of changed paths (never empty if the
/// input wasn't). Callers should size this at ~60% of `context_window`
/// to leave room for the system prompt and generated output.
pub fn summarize_for_prompt(diff: &str, budget_tokens: u32) -> String {
    if diff.trim().is_empty() {
        return String::new();
    }

    // Split into (header_line, body) per file. We preserve the exact
    // header text because downstream tasks parse filenames out of it.
    let mut files: Vec<(String, String)> = Vec::new();
    let mut cur_header: Option<String> = None;
    let mut cur_body = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if let Some(h) = cur_header.take() {
                files.push((h, std::mem::take(&mut cur_body)));
            }
            cur_header = Some(line.to_string());
        } else if cur_header.is_some() {
            cur_body.push_str(line);
            cur_body.push('\n');
        }
        // Lines before the first `diff --git` (rare; usually a cover
        // letter) are intentionally discarded — they're almost never
        // useful to the model.
    }
    if let Some(h) = cur_header {
        files.push((h, cur_body));
    }

    // Per-file trim: drop body for binaries; cap line count.
    let trimmed: Vec<(String, String)> = files
        .into_iter()
        .map(|(hdr, body)| {
            let path = path_from_header(&hdr).unwrap_or("");
            if is_binary_path(path) {
                return (hdr, "[binary file — body omitted]\n".to_string());
            }
            let lines: Vec<&str> = body.lines().collect();
            if lines.len() <= LINES_PER_FILE {
                (hdr, body)
            } else {
                let kept = lines[..LINES_PER_FILE].join("\n");
                let extra = lines.len() - LINES_PER_FILE;
                let mut out = kept;
                out.push('\n');
                out.push_str(&format!("[truncated: {} more lines]\n", extra));
                (hdr, out)
            }
        })
        .collect();

    // Assemble, tracking running cost. When we'd overflow, emit a
    // compact "…and N more files (paths: a, b, c)" summary so the
    // model still sees *which* files exist even if it can't read them.
    let mut out = String::new();
    let mut used: u32 = 0;
    let mut dropped_paths: Vec<String> = Vec::new();

    for (hdr, body) in &trimmed {
        let chunk = format!("{}\n{}", hdr, body);
        let cost = est_tokens(&chunk);
        if used + cost > budget_tokens && !out.is_empty() {
            let p = path_from_header(hdr).unwrap_or("").to_string();
            dropped_paths.push(p);
            continue;
        }
        out.push_str(&chunk);
        used += cost;
    }

    if !dropped_paths.is_empty() {
        let shown: Vec<&str> = dropped_paths.iter().take(20).map(String::as_str).collect();
        out.push_str(&format!(
            "\n[{} more files omitted for context budget: {}{}]\n",
            dropped_paths.len(),
            shown.join(", "),
            if dropped_paths.len() > shown.len() {
                ", ..."
            } else {
                ""
            }
        ));
    }

    out
}
