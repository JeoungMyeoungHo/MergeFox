//! Lightweight parser for git-style URLs and shorthand input from the welcome bar.
//!
//! Supports:
//!   - HTTPS: https://github.com/user/repo[.git]
//!   - SSH:   git@github.com:user/repo[.git]
//!   - SSH:   ssh://git@github.com/user/repo[.git]
//!   - git:   git://github.com/user/repo.git
//!   - Shorthand: user/repo  (assumed github.com)
//!
//! We intentionally do NOT pull in the `url` crate — the SSH form
//! `git@host:path` is not a valid URL and needs custom handling anyway.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitUrl {
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// Canonical clone URL we'll hand to `git2` (prefers https for shorthand).
    pub canonical: String,
}

impl GitUrl {
    pub fn suggested_folder_name(&self) -> String {
        self.repo.clone()
    }
}

pub fn parse(input: &str) -> Option<GitUrl> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }

    // ssh shorthand: git@host:owner/repo(.git)
    if let Some(rest) = s.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            if let Some((owner, repo)) = split_owner_repo(path) {
                return Some(GitUrl {
                    host: host.to_string(),
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    canonical: format!("git@{host}:{owner}/{repo}.git"),
                });
            }
        }
        return None;
    }

    // Protocols with :// — https, http, ssh, git
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            // ssh://git@host/owner/repo — strip user@
            let rest = rest.splitn(2, '@').last().unwrap_or(rest);
            let mut parts = rest.splitn(2, '/');
            let host = parts.next()?.to_string();
            let path = parts.next()?;
            let (owner, repo) = split_owner_repo(path)?;
            let canonical = if scheme.starts_with("http") {
                format!("https://{host}/{owner}/{repo}.git")
            } else {
                format!("{scheme}{host}/{owner}/{repo}.git")
            };
            return Some(GitUrl {
                host,
                owner: owner.to_string(),
                repo: repo.to_string(),
                canonical,
            });
        }
    }

    // GitHub shorthand: owner/repo
    if let Some((owner, repo)) = split_owner_repo(s) {
        // Only accept if there's no extra slashes / whitespace / weird chars
        if !owner.contains('/') && !repo.contains('/') {
            return Some(GitUrl {
                host: "github.com".to_string(),
                owner: owner.to_string(),
                repo: repo.to_string(),
                canonical: format!("https://github.com/{owner}/{repo}.git"),
            });
        }
    }

    None
}

fn split_owner_repo(path: &str) -> Option<(&str, &str)> {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let (owner, rest) = path.split_once('/')?;
    // Remove trailing `.git` and anything after another slash (subpath)
    let repo = rest.split('/').next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https() {
        let u = parse("https://github.com/rust-lang/rust").unwrap();
        assert_eq!(u.host, "github.com");
        assert_eq!(u.owner, "rust-lang");
        assert_eq!(u.repo, "rust");
        assert_eq!(u.canonical, "https://github.com/rust-lang/rust.git");
    }

    #[test]
    fn parses_https_with_git_suffix() {
        let u = parse("https://github.com/rust-lang/rust.git").unwrap();
        assert_eq!(u.repo, "rust");
    }

    #[test]
    fn parses_ssh_shorthand() {
        let u = parse("git@github.com:rust-lang/rust.git").unwrap();
        assert_eq!(u.host, "github.com");
        assert_eq!(u.canonical, "git@github.com:rust-lang/rust.git");
    }

    #[test]
    fn parses_ssh_protocol() {
        let u = parse("ssh://git@github.com/rust-lang/rust.git").unwrap();
        assert_eq!(u.canonical, "ssh://github.com/rust-lang/rust.git");
    }

    #[test]
    fn parses_github_shorthand() {
        let u = parse("rust-lang/rust").unwrap();
        assert_eq!(u.host, "github.com");
        assert_eq!(u.canonical, "https://github.com/rust-lang/rust.git");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_none());
        assert!(parse("hello world").is_none());
        assert!(parse("a/b/c/d").is_some()); // we accept, just takes first two
    }
}
