//! Remote-URL helpers + the [`Repo::remote_url`] method.
//!
//! Lives in its own file to keep `src/git/mod.rs` under the 1000-line per-file cap
//! (`module-size-cap`). The public API is re-exported through `super::*`.

use super::Repo;

impl Repo {
    /// URL of the `origin` remote, or `None` when no `origin` is configured.
    ///
    /// Used to derive a stable "scope" key for shared agent memory: two clones of the
    /// same repo into different workdirs should share memory entries.
    pub fn remote_url(&self) -> Option<String> {
        let local = self.local();
        let remote = local.try_find_remote("origin")?.ok()?;
        let url = remote.url(gix::remote::Direction::Fetch)?;
        Some(url.to_bstring().to_string())
    }
}

/// Normalise a git remote URL to a stable key suitable for memory-scope lookups.
///
/// Drops the `.git` suffix, trims trailing slashes, and lowercases the host portion
/// (the path remains case-sensitive). Both `git@github.com:Foo/bar.git` and
/// `https://github.com/Foo/bar/` collapse to `github.com/Foo/bar`.
pub fn normalize_remote_url(url: &str) -> String {
    let mut s = url.trim().to_string();
    while s.ends_with('/') {
        s.pop();
    }
    for prefix in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(at) = s.find('@')
        && !s[..at].contains('/')
    {
        s = s[at + 1..].to_string();
    }
    if let Some(colon) = s.find(':')
        && !s[..colon].contains('/')
    {
        s.replace_range(colon..=colon, "/");
    }
    if let Some(stripped) = s.strip_suffix(".git") {
        s = stripped.to_string();
    }
    while s.ends_with('/') {
        s.pop();
    }
    if let Some(slash) = s.find('/') {
        let host = s[..slash].to_ascii_lowercase();
        let rest = s[slash..].to_string();
        s = format!("{host}{rest}");
    } else {
        s = s.to_ascii_lowercase();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::normalize_remote_url;

    #[test]
    fn collapses_https_and_ssh_forms_to_same_key() {
        assert_eq!(
            normalize_remote_url("https://github.com/Foo/bar.git"),
            "github.com/Foo/bar"
        );
        assert_eq!(
            normalize_remote_url("git@github.com:Foo/bar.git"),
            "github.com/Foo/bar"
        );
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/Foo/bar.git/"),
            "github.com/Foo/bar"
        );
    }

    #[test]
    fn lowercases_host_but_preserves_path_case() {
        assert_eq!(
            normalize_remote_url("https://GitHub.COM/Foo/Bar"),
            "github.com/Foo/Bar"
        );
    }

    #[test]
    fn url_without_origin_remains_stable() {
        assert_eq!(normalize_remote_url("local-only"), "local-only");
    }
}
