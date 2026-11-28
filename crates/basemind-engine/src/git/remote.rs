//! Remote-URL helpers + the [`Repo::remote_url`] method.
//!
//! Lives in its own file to keep `src/git/mod.rs` under the 1000-line per-file cap
//! (`module-size-cap`). The public API is re-exported through `super::*`.

use super::Repo;

impl Repo {
    /// URL of the `origin` remote, or `None` when no `origin` is configured. [`scope_key`]
    /// derives the repository's scope from it.
    pub fn remote_url(&self) -> Option<String> {
        let local = self.local();
        let remote = local.try_find_remote("origin")?.ok()?;
        let url = remote.url(gix::remote::Direction::Fetch)?;
        Some(url.to_bstring().to_string())
    }
}

/// Derive the per-repo "scope" key used by the LanceDB tables, the registry and agent memory.
///
/// The normalised `origin` remote URL, else `path:<main worktree root>`, canonical, so every
/// worktree of a repository gets one key, with or without `origin`. For memory it is the scope a
/// note is filed under inside the store, and the store itself is per repository group
/// (`crate::store_layout::repo_group_key`, the same main worktree root): two clones at different
/// paths share a scope but not their notes. A note filed under an earlier scope is refiled under
/// this one on the first memory call (`crate::index::memory_store::rescope`).
pub fn scope_key(repo: &Repo) -> String {
    match repo.remote_url() {
        Some(url) => normalize_remote_url(&url),
        None => path_scope_key(&repo.main_worktree_root()),
    }
}

/// The scope of a root that is no git checkout, or of a checkout without `origin`: `path:<root>`.
/// The one place the shape is spelled; a caller that has a [`Repo`] passes the main worktree root
/// ([`scope_key`] does), one without leaves `root` as it was given.
pub fn path_scope_key(root: &std::path::Path) -> String {
    format!("path:{}", root.display())
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
    if !s.starts_with('[') {
        if let Some(colon) = s.find(':')
            && !s[..colon].contains('/')
        {
            let after = &s[colon + 1..];
            let port_len = after.bytes().take_while(u8::is_ascii_digit).count();
            if port_len > 0 && after[port_len..].starts_with('/') {
                s.replace_range(colon..colon + 1 + port_len, "");
            } else {
                s.replace_range(colon..=colon, "/");
            }
        }
    } else if let Some(bracket_end) = s.find(']') {
        let after_bracket = &s[bracket_end + 1..];
        if let Some(port_str) = after_bracket.strip_prefix(':') {
            let port_len = port_str.bytes().take_while(u8::is_ascii_digit).count();
            if port_len > 0 && port_str[port_len..].starts_with('/') {
                let colon_pos = bracket_end + 1;
                s.replace_range(colon_pos..colon_pos + 1 + port_len, "");
            }
        }
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
    use super::{Repo, normalize_remote_url, scope_key};

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = crate::testing::git_command()
            .args(["-c", "user.name=t", "-c", "user.email=t@e.x"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    }

    fn repo_with_linked_worktree(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let main = tmp.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        std::fs::write(main.join("a.txt"), b"a\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "--quiet", "-m", "init"]);
        let linked = tmp.join("linked");
        git(
            &main,
            &["worktree", "add", "--quiet", "-b", "side", linked.to_str().unwrap()],
        );
        (main, linked)
    }

    #[test]
    fn linked_worktrees_of_a_repo_without_origin_share_one_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, linked) = repo_with_linked_worktree(tmp.path());
        let from_main = scope_key(&Repo::discover(&main).unwrap());
        let from_linked = scope_key(&Repo::discover(&linked).unwrap());
        assert_eq!(from_main, from_linked);
        let canonical = main.canonicalize().unwrap();
        assert_eq!(from_main, format!("path:{}", canonical.display()));
    }

    #[test]
    fn a_repo_with_origin_keeps_its_normalised_remote_as_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, linked) = repo_with_linked_worktree(tmp.path());
        git(&main, &["remote", "add", "origin", "git@GitHub.com:Foo/bar.git"]);
        for root in [&main, &linked] {
            assert_eq!(scope_key(&Repo::discover(root).unwrap()), "github.com/Foo/bar");
        }
    }

    #[test]
    fn collapses_https_and_ssh_forms_to_same_key() {
        assert_eq!(
            normalize_remote_url("https://github.com/Foo/bar.git"),
            "github.com/Foo/bar"
        );
        assert_eq!(normalize_remote_url("git@github.com:Foo/bar.git"), "github.com/Foo/bar");
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/Foo/bar.git/"),
            "github.com/Foo/bar"
        );
    }

    #[test]
    fn ssh_with_port_normalizes_to_same_scope_as_https_and_scp() {
        let expected = "github.com/Foo/bar";
        assert_eq!(normalize_remote_url("ssh://git@github.com:22/Foo/bar"), expected);
        assert_eq!(normalize_remote_url("git@github.com:Foo/bar"), expected);
        assert_eq!(normalize_remote_url("https://github.com/Foo/bar"), expected);
        assert_eq!(normalize_remote_url("ssh://git@github.com:22/Foo/bar.git"), expected);
    }

    #[test]
    fn lowercases_host_but_preserves_path_case() {
        assert_eq!(normalize_remote_url("https://GitHub.COM/Foo/Bar"), "github.com/Foo/Bar");
    }

    #[test]
    fn url_without_origin_remains_stable() {
        assert_eq!(normalize_remote_url("local-only"), "local-only");
    }

    #[test]
    fn bracketed_ipv6_host_normalizes_correctly() {
        assert_eq!(
            normalize_remote_url("ssh://git@[2001:db8::1]:22/Foo/bar"),
            "[2001:db8::1]/Foo/bar"
        );
        assert_eq!(
            normalize_remote_url("ssh://git@[2001:db8::1]/Foo/bar"),
            "[2001:db8::1]/Foo/bar"
        );
        assert_eq!(
            normalize_remote_url("ssh://git@[2001:db8::1]:2222/Foo/bar.git"),
            "[2001:db8::1]/Foo/bar"
        );
    }
}
