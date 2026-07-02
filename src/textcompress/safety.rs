//! Security-critical primitives for output compression.
//!
//! This module owns the parts that MUST be conservative: credential detection,
//! error detection, and ANSI stripping. Everything here is built once via
//! [`std::sync::OnceLock`] so the hot path never recompiles a regex.
//!
//! The contract the rest of the crate relies on:
//! - [`strip_ansi`] removes escape codes but keeps the visible text intact.
//! - [`preserved_line_indices`] returns the set of line indices that carry a
//!   secret or an error marker; those lines are re-injected verbatim if a
//!   handler would otherwise drop them.
//! - [`looks_like_failure`] returns `true` when the text shows the command
//!   errored; the caller then fails open and returns the raw input unchanged.

use std::sync::OnceLock;

use ahash::AHashSet;
use regex::{Regex, RegexSet};

/// Compiled credential patterns. A line matching ANY of these is preserved
/// verbatim through compression — we never trade a secret for token savings.
///
/// The set mirrors the reference token-optimizer pattern list: AWS access
/// keys, OpenAI / Anthropic / HuggingFace / npm API keys, GitHub PATs, Stripe
/// keys, Slack tokens, Google keys, JWTs, PEM private-key headers, and database
/// / basic-auth URLs that embed credentials, plus the generic
/// `password=` / `token=` / `secret=` assignments the prompt calls out.
fn credential_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            r"AKIA[0-9A-Z]{16}",                                                 // AWS access key id
            r"ASIA[0-9A-Z]{16}",                                                 // AWS temp access key id
            r"sk-ant-[a-zA-Z0-9_\-]{20,}",                                       // Anthropic (before generic sk-)
            r"sk-[a-zA-Z0-9]{20,}",                                              // OpenAI / generic sk-
            r"ghp_[a-zA-Z0-9]{36}",                                              // GitHub personal access token
            r"gho_[a-zA-Z0-9]{36}",                                              // GitHub OAuth token
            r"ghu_[a-zA-Z0-9]{36}",                                              // GitHub user-to-server
            r"ghs_[a-zA-Z0-9]{36}",                                              // GitHub server-to-server
            r"ghr_[a-zA-Z0-9]{36}",                                              // GitHub refresh token
            r"github_pat_[a-zA-Z0-9_]{80,}",                                     // GitHub fine-grained PAT
            r"npm_[a-zA-Z0-9]{36}",                                              // npm token
            r"hf_[a-zA-Z0-9]{34}",                                               // HuggingFace token
            r"xox[baprs]-[0-9A-Za-z-]{10,}",                                     // Slack tokens
            r"sk_live_[a-zA-Z0-9]{24,}",                                         // Stripe live secret
            r"rk_live_[a-zA-Z0-9]{24,}",                                         // Stripe restricted live
            r"AIza[0-9A-Za-z_\-]{35}",                                           // Google API key
            r"ya29\.[0-9A-Za-z_\-]{20,}",                                        // Google OAuth access token
            r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}", // JWT
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",                               // PEM private key
            r"(?i)(?:postgres|postgresql|mysql|mongodb|mongodb\+srv|redis|amqp)://[^:\s/]+:[^@\s]+@", // db URL w/ creds
            r"(?i)https?://[^:\s/@]+:[^@\s]+@",                                  // basic-auth URL
            r"(?i)\b(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key)\s*[=:]\s*\S+", // generic assignment
            r"(?i)Bearer\s+[a-zA-Z0-9\-._~+/]+=*",                               // bearer token
        ])
        .expect("static credential patterns compile")
    })
}

/// Compiled error-marker patterns. A line matching one of these signals failure
/// and is BOTH (a) preserved verbatim if a handler would drop it, and (b) when
/// present anywhere in the text, used by [`looks_like_failure`] to fail open.
///
/// The patterns are deliberately specific so common English vocabulary
/// (`no errors`, `error-free`) does not trip every line: a trailing colon
/// (`error:`, `fatal:`), a whole word (`exception`, `traceback`, `panic`), a
/// bang form (`npm ERR!`), a bracketed Rust diagnostic (`error[E0599]`), a
/// named error class (`ValueError`, `ModuleNotFoundError`), a shell failure
/// phrase (`command not found`, `permission denied`), or a fatal signal
/// (`SIGKILL`, `Killed`, `Aborted`).
fn error_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            r"(?i)\berror\s*:",
            r"(?i)\bfatal\s*:",
            r"(?i)\bpanic\s*:",
            r"(?i)\bexception\b",
            r"(?i)\btraceback\b",
            r"\bFAILED\b",
            r"(?i)\bsegmentation fault\b",
            r"(?i)\bunhandled\b",
            // npm-style failures carry no trailing colon (`npm ERR!`).
            r"(?i)\bnpm\s+err!",
            // Named runtime/diagnostic error classes (Python, JS, generic runtimes).
            r"(?i)\b(?:Syntax|Value|Key|Type|File\s*Not\s*Found|Module\s*Not\s*Found|Runtime|Reference|Range)Error\b",
            // Rust bracketed diagnostics, e.g. `error[E0599]`.
            r"\berror\[",
            // Shell / OS failure phrases.
            r"(?i)\b(?:command not found|permission denied|no such file or directory)\b",
            // Fatal signals / process-death markers.
            r"(?i)\b(?:SIGKILL|SIGABRT|SIGSEGV|Killed|Aborted)\b",
        ])
        .expect("static error patterns compile")
    })
}

/// CSI escape sequences (colour / style / cursor moves).
fn ansi_csi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("ansi csi compiles"))
}

/// OSC 8 hyperlinks. Captured group 1 is the visible label, which we keep so a
/// credential embedded in the visible text survives the pre-compression scan.
fn ansi_osc8_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\]8;[^\x07]*\x07([^\x1b]*)\x1b\]8;;\x07").expect("ansi osc8 compiles"))
}

/// Remove ANSI escape codes while preserving visible text.
///
/// Two passes: first collapse OSC 8 hyperlinks to their visible label (so a
/// credential hidden in the label is not dropped with the link structure), then
/// strip CSI sequences. Returns an owned `String`; when the input has no escape
/// codes the cost is one allocation, which is acceptable on this cold path.
pub fn strip_ansi(text: &str) -> String {
    // Fast path: no ESC byte means nothing to strip.
    if memchr::memchr(0x1b, text.as_bytes()).is_none() {
        return text.to_string();
    }
    let delinked = ansi_osc8_re().replace_all(text, "$1");
    ansi_csi_re().replace_all(&delinked, "").into_owned()
}

/// Return the set of 0-based line indices that MUST survive compression:
/// any line carrying a credential or an error marker.
///
/// Used by the orchestrator to re-inject dropped lines after a handler runs.
pub fn preserved_line_indices(text: &str) -> AHashSet<usize> {
    let creds = credential_set();
    let errors = error_set();
    let mut preserved = AHashSet::new();
    for (i, line) in text.lines().enumerate() {
        if creds.is_match(line) || errors.is_match(line) {
            preserved.insert(i);
        }
    }
    preserved
}

/// Return `true` when any line carries a credential.
///
/// Used by the orchestrator as a final backstop so credential-bearing output is
/// never silently compressed away even when no handler would have dropped it.
pub fn contains_credential(text: &str) -> bool {
    let creds = credential_set();
    text.lines().any(|line| creds.is_match(line))
}

/// Return `true` when a single `line` carries an error marker.
///
/// Reuses the same compiled [`error_set`] that [`looks_like_failure`] and
/// [`preserved_line_indices`] consult, so callers (e.g. checkpoint extraction)
/// classify a line as an error without duplicating the pattern list.
pub fn is_error_line(line: &str) -> bool {
    error_set().is_match(line)
}

/// Return `true` when the text shows the command failed and its output must NOT
/// be compressed (fail-open). Triggers on any error marker anywhere in the body.
///
/// This is intentionally aggressive: a false positive merely passes the raw
/// output through (no token savings, but no lost detail), whereas a false
/// negative could drop the very lines an agent needs to debug.
pub fn looks_like_failure(text: &str) -> bool {
    error_set().is_match(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_keeps_visible_text() {
        let colored = "\x1b[31mred error\x1b[0m tail";
        assert_eq!(strip_ansi(colored), "red error tail");
    }

    #[test]
    fn strip_ansi_keeps_osc8_label() {
        let link = "before \x1b]8;;https://x\x07ghp_visiblelabel\x1b]8;;\x07 after";
        assert_eq!(strip_ansi(link), "before ghp_visiblelabel after");
    }

    #[test]
    fn strip_ansi_noop_without_escape() {
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn detects_aws_key_line() {
        let idx = preserved_line_indices("clean line\nkey AKIAIOSFODNN7EXAMPLE here\nmore");
        assert!(idx.contains(&1), "AWS key line must be preserved");
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn detects_github_pat_and_private_key() {
        let pat = format!("token {} done", "ghp_".to_string() + &"a".repeat(36));
        assert!(contains_credential(&pat), "ghp_ PAT must match");
        assert!(
            contains_credential("-----BEGIN RSA PRIVATE KEY-----"),
            "PEM header must match"
        );
    }

    #[test]
    fn detects_generic_secret_assignment() {
        assert!(contains_credential("export password=hunter2"));
        assert!(contains_credential("API_KEY: abc123def456"));
        assert!(!contains_credential("the password field is empty"));
    }

    #[test]
    fn looks_like_failure_on_error_marker() {
        assert!(looks_like_failure("warming up\nerror: build failed\ndone"));
        assert!(looks_like_failure("Traceback (most recent call last):"));
        assert!(!looks_like_failure("everything is fine\nno problems here"));
    }

    #[test]
    fn looks_like_failure_on_npm_err_bang() {
        assert!(looks_like_failure("npm ERR! code ELIFECYCLE\nnpm ERR! errno 1"));
    }

    #[test]
    fn looks_like_failure_on_named_error_classes() {
        assert!(looks_like_failure("SyntaxError: unexpected token"));
        assert!(looks_like_failure("ValueError"));
        assert!(looks_like_failure("KeyError"));
        assert!(looks_like_failure("TypeError"));
        assert!(looks_like_failure("FileNotFoundError"));
        assert!(looks_like_failure("ModuleNotFoundError"));
        assert!(looks_like_failure("RuntimeError"));
        assert!(looks_like_failure("ReferenceError"));
        assert!(looks_like_failure("RangeError"));
    }

    #[test]
    fn looks_like_failure_on_rust_bracketed_diagnostic() {
        assert!(looks_like_failure("error[E0599]: no method named foo"));
    }

    #[test]
    fn looks_like_failure_on_shell_phrases() {
        assert!(looks_like_failure("bash: frobnicate: command not found"));
        assert!(looks_like_failure("open config: permission denied"));
        assert!(looks_like_failure("cat x: No such file or directory"));
    }

    #[test]
    fn looks_like_failure_on_fatal_signals() {
        assert!(looks_like_failure("worker received SIGKILL"));
        assert!(looks_like_failure("trace trap: SIGABRT"));
        assert!(looks_like_failure("Segfault SIGSEGV at 0x0"));
        assert!(looks_like_failure("Killed"));
        assert!(looks_like_failure("Aborted (core dumped)"));
    }
}
