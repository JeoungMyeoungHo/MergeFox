//! `git blame --porcelain` parser.
//!
//! Blame is implemented as a CLI shell-out (not gix) because the
//! canonical blame algorithm — diff-based with rename detection and
//! `--incremental` streaming — is non-trivial to reimplement correctly.
//! `git blame` itself is fast enough (under 100 ms on typical files)
//! that a subprocess round-trip is not the bottleneck; call it from a
//! background thread so the UI stays responsive on large files.
//!
//! Output shape we parse:
//!
//! ```text
//! <40-hex sha> <orig-line> <final-line> <num-lines>
//! author Foo Bar
//! author-mail <foo@bar.com>
//! author-time 1700000000
//! author-tz +0900
//! ...
//! summary Subject line of the commit
//! filename some/path.rs
//! \t<actual source line content>
//! ```
//!
//! The first header line per hunk carries the SHA + line numbers; the
//! metadata lines follow until the `\t`-prefixed content line. Repeat
//! hunks (same SHA) only print the SHA + line numbers, no metadata.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

/// One annotated line of the blamed file, in output order.
#[derive(Debug, Clone)]
pub struct BlameLine {
    pub line_no: u32,
    pub content: String,
    pub commit: BlameCommit,
}

/// Deduplicated per-SHA metadata. Cheap to hand out as a reference
/// into the full `BlameResult` from the UI.
#[derive(Debug, Clone, Default)]
pub struct BlameCommit {
    pub sha: String,
    pub author: String,
    pub author_email: String,
    /// Unix seconds.
    pub author_time: i64,
    /// Commit subject line ("summary" in porcelain).
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct BlameResult {
    pub lines: Vec<BlameLine>,
    pub path: std::path::PathBuf,
}

/// Run `git blame --porcelain -w` on the given file and return the
/// parsed annotation list. `-w` ignores whitespace-only changes when
/// assigning blame (matches most users' intuition for "who wrote
/// this line").
pub fn blame_file(repo_path: &Path, file: &Path) -> Result<BlameResult> {
    let file_str = file.to_string_lossy().into_owned();
    let out = super::cli::run(
        repo_path,
        ["blame", "--porcelain", "-w", "--", &file_str],
    )?;
    let text = out.stdout_str();
    Ok(BlameResult {
        lines: parse_porcelain(&text),
        path: file.to_path_buf(),
    })
}

/// Parse porcelain blame output. Extracted so a unit test can exercise
/// it without spawning `git`.
pub fn parse_porcelain(text: &str) -> Vec<BlameLine> {
    let mut commits: HashMap<String, BlameCommit> = HashMap::new();
    let mut out: Vec<BlameLine> = Vec::new();

    // Header parse state: after we see a header line we collect
    // metadata until the content line.
    let mut current_sha: Option<String> = None;
    let mut current_final_line: u32 = 0;
    let mut pending_meta: BlameCommit = BlameCommit::default();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('\t') {
            // Content line — terminates the current hunk entry.
            if let Some(sha) = current_sha.clone() {
                // Did we see metadata for this hunk? The first time a
                // SHA appears in the porcelain stream it's followed by
                // author/author-mail/summary/… lines. Subsequent hunks
                // with the same SHA skip the metadata block, so we
                // fall back to whatever we cached the first time.
                let saw_metadata = !pending_meta.author.is_empty()
                    || !pending_meta.summary.is_empty()
                    || pending_meta.author_time != 0;
                let commit = if saw_metadata {
                    pending_meta.sha = sha.clone();
                    commits.insert(sha.clone(), pending_meta.clone());
                    pending_meta.clone()
                } else {
                    commits.get(&sha).cloned().unwrap_or(BlameCommit {
                        sha: sha.clone(),
                        ..BlameCommit::default()
                    })
                };
                out.push(BlameLine {
                    line_no: current_final_line,
                    content: rest.to_string(),
                    commit,
                });
            }
            pending_meta = BlameCommit::default();
            current_sha = None;
            continue;
        }
        // Header or metadata line.
        if is_header_line(line) {
            // `<sha> <orig> <final> <count>` — we only care about sha + final
            let mut it = line.split_whitespace();
            let sha = it.next().unwrap_or("").to_string();
            let _orig = it.next();
            let final_line = it
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            current_sha = Some(sha);
            current_final_line = final_line;
            pending_meta = BlameCommit::default();
            continue;
        }
        // Key-value metadata.
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "author" => pending_meta.author = value.to_string(),
            "author-mail" => {
                pending_meta.author_email =
                    value.trim_start_matches('<').trim_end_matches('>').to_string();
            }
            "author-time" => {
                pending_meta.author_time = value.parse::<i64>().unwrap_or(0);
            }
            "summary" => pending_meta.summary = value.to_string(),
            _ => {}
        }
    }
    out
}

/// Is this the `<sha> <orig> <final> [count]` header line?
/// Detected by: first token is a 40-char hex string.
fn is_header_line(line: &str) -> bool {
    let Some(first) = line.split_whitespace().next() else {
        return false;
    };
    first.len() == 40 && first.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_two_line_output() {
        let input = concat!(
            "0123456789abcdef0123456789abcdef01234567 1 1 2\n",
            "author Alice\n",
            "author-mail <alice@example.com>\n",
            "author-time 1700000000\n",
            "summary first commit\n",
            "filename src/lib.rs\n",
            "\tfn main() {}\n",
            "0123456789abcdef0123456789abcdef01234567 2 2\n",
            "\t\n",
        );
        let lines = parse_porcelain(input);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line_no, 1);
        assert_eq!(lines[0].content, "fn main() {}");
        assert_eq!(lines[0].commit.author, "Alice");
        assert_eq!(lines[0].commit.author_email, "alice@example.com");
        assert_eq!(lines[0].commit.summary, "first commit");
        // Repeated hunk reuses the cached metadata.
        assert_eq!(lines[1].commit.author, "Alice");
        assert_eq!(lines[1].line_no, 2);
    }
}
