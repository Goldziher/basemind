//! Command-family detection from output shape.
//!
//! When the caller does not pass an explicit `--family`, we sniff the family
//! from the *text itself* (not a command line — this tool only ever sees
//! output). Detection is conservative: an unrecognised shape returns
//! [`Family::Logs`], the generic duplicate-collapsing handler, which is always
//! safe.

use std::sync::OnceLock;

use memchr::memmem::Finder;
use regex::Regex;

/// The command families this version compresses. Each maps to one handler in
/// `crate::textcompress::handlers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `git status` long form → one-line branch + counts.
    GitStatus,
    /// `git log` → strip GPG / merge noise.
    GitLog,
    /// `git diff` → header + stats + truncated body.
    GitDiff,
    /// `npm install` / `pip install` → keep added/removed/audited/vuln counts.
    NpmInstall,
    /// `cargo build` → keep warnings/errors + final summary.
    CargoBuild,
    /// pytest / generic test runners → summary line + FAILURES section.
    Pytest,
    /// `ls` / `find` long listings → head N + `(M more)`.
    Ls,
    /// `grep` / `rg` → group by file, top-N matches per file.
    Grep,
    /// Generic logs → collapse consecutive duplicate lines to `(xN)`.
    Logs,
}

impl Family {
    /// Parse an explicit `--family` value. Accepts a few aliases per family so
    /// callers don't have to remember the exact spelling. Returns `None` for an
    /// unknown name so the CLI can report a usage error.
    pub fn parse(name: &str) -> Option<Self> {
        let n = name.trim().to_ascii_lowercase();
        let family = match n.as_str() {
            "git_status" | "git-status" | "gitstatus" => Family::GitStatus,
            "git_log" | "git-log" | "gitlog" => Family::GitLog,
            "git_diff" | "git-diff" | "gitdiff" | "diff" => Family::GitDiff,
            "npm_install" | "npm-install" | "npm" | "pip" | "pip_install" | "install" => {
                Family::NpmInstall
            }
            "cargo_build" | "cargo-build" | "cargo" | "build" => Family::CargoBuild,
            "pytest" | "test" | "tests" | "jest" => Family::Pytest,
            "ls" | "find" | "listing" => Family::Ls,
            "grep" | "rg" | "ripgrep" | "search" => Family::Grep,
            "logs" | "log" | "generic" => Family::Logs,
            _ => return None,
        };
        Some(family)
    }

    /// Stable lowercase name reported back in [`crate::textcompress::CompressionOutcome::family_detected`].
    pub fn as_str(self) -> &'static str {
        match self {
            Family::GitStatus => "git_status",
            Family::GitLog => "git_log",
            Family::GitDiff => "git_diff",
            Family::NpmInstall => "npm_install",
            Family::CargoBuild => "cargo_build",
            Family::Pytest => "pytest",
            Family::Ls => "ls",
            Family::Grep => "grep",
            Family::Logs => "logs",
        }
    }
}

fn finder(needle: &'static str) -> Finder<'static> {
    Finder::new(needle.as_bytes())
}

/// `file:line:` prefix, the grep/ripgrep shape. Non-greedy path up to `:N:`.
fn grep_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.+?):(\d+):").expect("grep line re compiles"))
}

/// Detect the family from the output shape. Built-once `memmem::Finder`s drive
/// the fixed-substring sniffs; the grep shape needs a regex because it is a
/// positional pattern, not a fixed substring.
///
/// Order matters: more specific shapes are checked before the generic
/// `Logs` fallback.
pub fn detect(text: &str) -> Family {
    static GIT_STATUS_BRANCH: OnceLock<Finder<'static>> = OnceLock::new();
    static GIT_STATUS_CLEAN: OnceLock<Finder<'static>> = OnceLock::new();
    static GIT_DIFF: OnceLock<Finder<'static>> = OnceLock::new();
    static GIT_COMMIT: OnceLock<Finder<'static>> = OnceLock::new();
    static CARGO: OnceLock<Finder<'static>> = OnceLock::new();
    static NPM_AUDIT: OnceLock<Finder<'static>> = OnceLock::new();
    static NPM_ADDED: OnceLock<Finder<'static>> = OnceLock::new();
    static PIP_INSTALLED: OnceLock<Finder<'static>> = OnceLock::new();
    static PYTEST: OnceLock<Finder<'static>> = OnceLock::new();

    let bytes = text.as_bytes();

    // git status: "On branch X" header or the clean-tree sentinel.
    let branch = GIT_STATUS_BRANCH.get_or_init(|| finder("On branch "));
    let clean = GIT_STATUS_CLEAN.get_or_init(|| finder("nothing to commit"));
    if branch.find(bytes).is_some() || clean.find(bytes).is_some() {
        return Family::GitStatus;
    }

    // git diff: the `diff --git` header.
    let diff = GIT_DIFF.get_or_init(|| finder("diff --git "));
    if diff.find(bytes).is_some() {
        return Family::GitDiff;
    }

    // git log: lines starting with "commit <sha>".
    let commit = GIT_COMMIT.get_or_init(|| finder("commit "));
    if text
        .lines()
        .any(|l| commit.find(l.as_bytes()) == Some(0) && is_git_log_commit_line(l))
    {
        return Family::GitLog;
    }

    // cargo build: "Compiling " / "Finished " emitted by cargo.
    let cargo = CARGO.get_or_init(|| finder("Compiling "));
    if cargo.find(bytes).is_some() {
        return Family::CargoBuild;
    }

    // npm install: "added N packages" / "audited" lines.
    let added = NPM_ADDED.get_or_init(|| finder("added "));
    let audit = NPM_AUDIT.get_or_init(|| finder("audited "));
    if audit.find(bytes).is_some() || added.find(bytes).is_some() {
        return Family::NpmInstall;
    }
    // pip install: "Successfully installed".
    let pip = PIP_INSTALLED.get_or_init(|| finder("Successfully installed"));
    if pip.find(bytes).is_some() {
        return Family::NpmInstall;
    }

    // pytest: the "passed" / "failed" summary tokens with the "=" rule lines.
    let pytest = PYTEST.get_or_init(|| finder(" passed"));
    if pytest.find(bytes).is_some() && text.contains("===") {
        return Family::Pytest;
    }
    if text.contains("test result:") {
        // cargo test summary.
        return Family::Pytest;
    }

    // grep/rg: a majority of lines match `file:line:`.
    if looks_like_grep(text) {
        return Family::Grep;
    }

    // ls/find: many lines that look like bare paths/filenames.
    if looks_like_listing(text) {
        return Family::Ls;
    }

    Family::Logs
}

/// A `git log` commit line is `commit <40-hex>` (optionally with ` (HEAD ...)`).
fn is_git_log_commit_line(line: &str) -> bool {
    let rest = &line["commit ".len()..];
    let sha: &str = rest.split_whitespace().next().unwrap_or("");
    sha.len() >= 7 && sha.len() <= 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Heuristic: at least 30% of non-empty lines match the `file:line:` grep shape,
/// over a meaningful minimum, before we treat output as search results.
fn looks_like_grep(text: &str) -> bool {
    let re = grep_line_re();
    let mut total = 0usize;
    let mut hits = 0usize;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        total += 1;
        if re.is_match(line) {
            hits += 1;
        }
    }
    total >= 5 && hits * 10 >= total * 3
}

/// Heuristic: most non-empty lines are short, single-token, slash-or-name
/// fragments with no embedded spaces beyond an `ls -l`-style columnar prefix.
/// Conservative — we only claim a listing when the bulk of lines look path-like.
fn looks_like_listing(text: &str) -> bool {
    let mut total = 0usize;
    let mut pathish = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        total += 1;
        // Path-like: no whitespace, or `ls -l` long form (perms in col 0).
        let single_token = !t.contains(char::is_whitespace);
        let ls_long = t.len() > 10
            && (t.starts_with('-') || t.starts_with('d') || t.starts_with('l'))
            && t.as_bytes()[1..10]
                .iter()
                .all(|&b| matches!(b, b'r' | b'w' | b'x' | b's' | b't' | b'-' | b'@' | b'+'));
        if single_token || ls_long {
            pathish += 1;
        }
    }
    total >= 10 && pathish * 4 >= total * 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aliases() {
        assert_eq!(Family::parse("git-status"), Some(Family::GitStatus));
        assert_eq!(Family::parse("RG"), Some(Family::Grep));
        assert_eq!(Family::parse("nope"), None);
    }

    #[test]
    fn detect_git_status() {
        assert_eq!(
            detect("On branch main\nnothing to commit"),
            Family::GitStatus
        );
    }

    #[test]
    fn detect_git_diff() {
        assert_eq!(
            detect("diff --git a/x b/x\n+++ b/x\n+added"),
            Family::GitDiff
        );
    }

    #[test]
    fn detect_git_log() {
        let log = "commit 1234567890abcdef1234567890abcdef12345678\nAuthor: x\n\n    msg\n";
        assert_eq!(detect(log), Family::GitLog);
    }

    #[test]
    fn detect_cargo_and_npm() {
        assert_eq!(detect("   Compiling foo v0.1.0\n"), Family::CargoBuild);
        assert_eq!(detect("added 120 packages in 3s\n"), Family::NpmInstall);
    }

    #[test]
    fn detect_grep_shape() {
        let g = "src/a.rs:10:foo\nsrc/a.rs:11:bar\nsrc/b.rs:3:baz\nsrc/b.rs:4:qux\nsrc/c.rs:1:x";
        assert_eq!(detect(g), Family::Grep);
    }

    #[test]
    fn detect_falls_back_to_logs() {
        assert_eq!(
            detect("some arbitrary prose that matches nothing"),
            Family::Logs
        );
    }
}
