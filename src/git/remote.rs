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

/// Derive the per-repo "scope" key used by the LanceDB tables and shared agent memory.
///
/// Prefers the normalised `origin` remote URL — stable across clones, machines, and
/// directory moves. Falls back to `path:<workdir-realpath>` when no remote is configured
/// (e.g. local-only experiments).
pub fn scope_key(repo: &Repo) -> String {
    match repo.remote_url() {
        Some(url) => normalize_remote_url(&url),
        None => format!("path:{}", repo.workdir().display()),
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
    // Strip an explicit `:<port>` segment in the host position before the scp-form colon
    // rewrite. Without this, `ssh://git@github.com:22/Foo/bar` (port form) rewrites to
    // `github.com/22/Foo/bar`, diverging from the https form's `github.com/Foo/bar` and
    // splitting the memory scope across ssh+port vs https clones of the same repo.
    //
    // Bracketed IPv6 hosts (`[2001:db8::1]:22/path`) must be left alone — the colons inside
    // `[...]` are part of the address, not port delimiters. Detect and skip them entirely so
    // `ssh://git@[2001:db8::1]:22/Foo/bar` collapses to `[2001:db8::1]/Foo/bar` rather than
    // being corrupted by the port-strip or scp-colon logic.
    if !s.starts_with('[') {
        if let Some(colon) = s.find(':')
            && !s[..colon].contains('/')
        {
            let after = &s[colon + 1..];
            let port_len = after.bytes().take_while(u8::is_ascii_digit).count();
            if port_len > 0 && after[port_len..].starts_with('/') {
                // `host:22/path` → `host/path`: drop the colon and the digit run, keep the slash.
                s.replace_range(colon..colon + 1 + port_len, "");
            } else {
                // scp form `host:path` → `host/path`: turn the single colon into a slash.
                s.replace_range(colon..=colon, "/");
            }
        }
    } else if let Some(bracket_end) = s.find(']') {
        // Bracketed IPv6: strip the optional `:<port>` immediately after the closing `]`.
        let after_bracket = &s[bracket_end + 1..];
        if let Some(port_str) = after_bracket.strip_prefix(':') {
            let port_len = port_str.bytes().take_while(u8::is_ascii_digit).count();
            if port_len > 0 && port_str[port_len..].starts_with('/') {
                // `[::1]:22/path` → `[::1]/path`
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
    use super::normalize_remote_url;

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
        // `.git` suffix + ssh port should still collapse identically.
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
        // `ssh://git@[2001:db8::1]:22/Foo/bar` — the colons inside `[...]` are part of the
        // IPv6 address and must not be treated as port delimiters or scp-form separators.
        // The host portion is lowercased (no effect for hex-digit IPv6 addresses); the path
        // component is preserved case-sensitively, consistent with the non-IPv6 behavior.
        assert_eq!(
            normalize_remote_url("ssh://git@[2001:db8::1]:22/Foo/bar"),
            "[2001:db8::1]/Foo/bar"
        );
        // Without port: brackets but no port suffix.
        assert_eq!(
            normalize_remote_url("ssh://git@[2001:db8::1]/Foo/bar"),
            "[2001:db8::1]/Foo/bar"
        );
        // With .git suffix.
        assert_eq!(
            normalize_remote_url("ssh://git@[2001:db8::1]:2222/Foo/bar.git"),
            "[2001:db8::1]/Foo/bar"
        );
    }
}
