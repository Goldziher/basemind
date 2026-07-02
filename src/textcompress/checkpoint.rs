//! Checkpoint extraction (token-reduction workstream W6).
//!
//! Distil accumulated session text — a transcript chunk or concatenated tool
//! output piped on stdin — into a compact, credential-safe, structured
//! [`Checkpoint`]: the **decisions**, **errors**, and **changed files**. A
//! hook or agent can persist or restore this instead of re-reading the whole
//! session.
//!
//! Two hard rules shape the design:
//!
//! 1. **A checkpoint is re-injected context, so it must never carry a
//!    credential.** Any candidate line for which
//!    `safety::contains_credential` is true is dropped entirely (omitted, not
//!    redacted-in-place) before it can land in any field.
//! 2. **Changed files come from git, not regex.** The pure
//!    [`extract_checkpoint`] takes the file list as an injected argument; the
//!    CLI runner fetches it from the git working tree. This module performs no
//!    git I/O and holds no global state.

use std::sync::OnceLock;

use ahash::AHashSet;
use regex::RegexSet;
use serde::Serialize;

use super::safety;

/// Maximum number of decision lines retained; the rest are truncated and
/// [`Checkpoint::decisions_truncated`] is set.
const MAX_DECISIONS: usize = 50;
/// Maximum number of error lines retained; the rest are truncated and
/// [`Checkpoint::errors_truncated`] is set.
const MAX_ERRORS: usize = 50;
/// Maximum number of changed-file paths retained.
const MAX_FILES: usize = 200;

/// A compact, credential-safe summary of a session's accumulated text.
///
/// `decisions` and `errors` are deduplicated, credential-stripped lines drawn
/// from the input text; `files_changed` is the deduplicated, capped git
/// working-tree change list injected by the caller. The `*_truncated` flags are
/// set when a list exceeded its cap, so a consumer never mistakes a capped list
/// for a complete one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Checkpoint {
    /// Lines that record a decision (see the decision marker set).
    pub decisions: Vec<String>,
    /// Lines that record an error (see `safety::is_error_line`).
    pub errors: Vec<String>,
    /// Working-tree paths that changed, as supplied by the caller.
    pub files_changed: Vec<String>,
    /// `true` when more than `MAX_DECISIONS` decision lines were found.
    pub decisions_truncated: bool,
    /// `true` when more than `MAX_ERRORS` error lines were found.
    pub errors_truncated: bool,
}

/// Conservative decision-marker patterns. Deliberately specific so ordinary
/// prose does not trip every line: a whole word (`decided`, `decision`,
/// `chose`), a modal-decision phrase (`we will`, `we should`, `going with`,
/// `opt(ed|ing) for`), a `conclusion:` lead-in, or an actionable annotation
/// (`TODO`, `FIXME`). Built once via [`OnceLock`], mirroring `safety.rs`.
fn decision_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            r"(?i)\bdecided\b",
            r"(?i)\bdecision\b",
            r"(?i)\bchose\b|\bchoosing\b",
            r"(?i)\bwe (?:will|should|must|chose|decided)\b",
            r"(?i)\bgoing with\b",
            r"(?i)\bopt(?:ed|ing) for\b",
            r"(?i)\bconclusion\s*:",
            r"\bTODO\b",
            r"\bFIXME\b",
        ])
        .expect("static decision patterns compile")
    })
}

/// Return `true` when `line` records a decision per [`decision_set`].
fn is_decision_line(line: &str) -> bool {
    decision_set().is_match(line)
}

/// Extract a [`Checkpoint`] from session `text` and a pre-fetched
/// `files_changed` list.
///
/// Pipeline:
/// 1. ANSI is stripped via `safety::strip_ansi` so a coloured marker still
///    matches.
/// 2. Each trimmed, non-blank line is classified: decision lines via the
///    decision marker set, error lines via `safety::is_error_line`.
/// 3. **Credential gate** — any line for which
///    `safety::contains_credential` is true is dropped entirely before
///    classification can retain it.
/// 4. Exact-duplicate lines are collapsed within each list (first-seen order),
///    and the injected `files_changed` list is likewise trimmed/deduplicated.
/// 5. Each list is capped (`MAX_DECISIONS` / `MAX_ERRORS` / `MAX_FILES`);
///    a list that exceeded its cap sets the matching `*_truncated` flag.
pub fn extract_checkpoint(text: &str, files_changed: Vec<String>) -> Checkpoint {
    let cleaned = safety::strip_ansi(text);

    let mut decisions: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut decisions_seen: AHashSet<&str> = AHashSet::new();
    let mut errors_seen: AHashSet<&str> = AHashSet::new();
    let mut decisions_truncated = false;
    let mut errors_truncated = false;

    for raw in cleaned.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Hard security gate: a credential-bearing line never enters a field.
        if safety::contains_credential(line) {
            continue;
        }

        if is_decision_line(line) && decisions_seen.insert(line) {
            if decisions.len() < MAX_DECISIONS {
                decisions.push(line.to_string());
            } else {
                decisions_truncated = true;
            }
        }
        if safety::is_error_line(line) && errors_seen.insert(line) {
            if errors.len() < MAX_ERRORS {
                errors.push(line.to_string());
            } else {
                errors_truncated = true;
            }
        }
    }

    let files_changed = dedup_and_cap_files(files_changed);

    Checkpoint {
        decisions,
        errors,
        files_changed,
        decisions_truncated,
        errors_truncated,
    }
}

/// Trim, drop blanks, deduplicate (first-seen order), and cap the injected file
/// list at [`MAX_FILES`]. The pure fn does not error on overflow — it simply
/// caps, matching the silent-cap-free contract of the decision/error lists
/// (file overflow has no dedicated flag because the git status is already an
/// upper-bounded set in practice).
fn dedup_and_cap_files(files: Vec<String>) -> Vec<String> {
    let mut seen: AHashSet<String> = AHashSet::new();
    let mut out: Vec<String> = Vec::new();
    for path in files {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        if out.len() >= MAX_FILES {
            break;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_decision_line() {
        let cp = extract_checkpoint("We decided to use Fjall.", Vec::new());
        assert_eq!(cp.decisions, vec!["We decided to use Fjall.".to_string()]);
        assert!(cp.errors.is_empty());
        assert!(!cp.decisions_truncated);
    }

    #[test]
    fn extracts_error_line_via_is_error_line() {
        let cp = extract_checkpoint("error: build failed", Vec::new());
        assert_eq!(cp.errors, vec!["error: build failed".to_string()]);
        assert!(cp.decisions.is_empty());
    }

    #[test]
    fn drops_credential_line_from_decisions_and_errors() {
        // A line that is BOTH a decision marker AND carries a GitHub PAT must be
        // dropped from every field — the hard security gate.
        let pat = format!("chose token=ghp_{}", "a".repeat(36));
        let cp = extract_checkpoint(&pat, Vec::new());
        assert!(
            cp.decisions.is_empty(),
            "credential decision line must be dropped, got: {:?}",
            cp.decisions
        );
        assert!(
            cp.errors.is_empty(),
            "credential error line must be dropped, got: {:?}",
            cp.errors
        );
        // The token must not appear anywhere in any field.
        let pat_token = format!("ghp_{}", "a".repeat(36));
        assert!(
            !cp.decisions.iter().any(|d| d.contains(&pat_token)),
            "PAT leaked into decisions"
        );
        assert!(
            !cp.errors.iter().any(|e| e.contains(&pat_token)),
            "PAT leaked into errors"
        );
        assert!(
            !cp.files_changed.iter().any(|f| f.contains(&pat_token)),
            "PAT leaked into files_changed"
        );
    }

    #[test]
    fn drops_credential_error_line() {
        // An error-marked line carrying an AWS key must also be dropped.
        let line = "error: leaked AKIAIOSFODNN7EXAMPLE in config";
        let cp = extract_checkpoint(line, Vec::new());
        assert!(
            cp.errors.is_empty(),
            "credential error line must be dropped"
        );
        assert!(
            !cp.errors.iter().any(|e| e.contains("AKIAIOSFODNN7EXAMPLE")),
            "AWS key leaked into errors"
        );
    }

    #[test]
    fn dedup_collapses_duplicate_decisions() {
        let text = "We decided to ship.\nWe decided to ship.\nWe decided to ship.";
        let cp = extract_checkpoint(text, Vec::new());
        assert_eq!(cp.decisions, vec!["We decided to ship.".to_string()]);
        assert!(!cp.decisions_truncated);
    }

    #[test]
    fn caps_and_flags_truncated_errors() {
        let mut text = String::new();
        for n in 0..(MAX_ERRORS + 10) {
            // Unique line per iteration so dedup does not collapse them.
            text.push_str(&format!("error: failure number {n}\n"));
        }
        let cp = extract_checkpoint(&text, Vec::new());
        assert_eq!(cp.errors.len(), MAX_ERRORS);
        assert!(cp.errors_truncated, "errors_truncated must fire past cap");
    }

    #[test]
    fn caps_and_flags_truncated_decisions() {
        let mut text = String::new();
        for n in 0..(MAX_DECISIONS + 5) {
            text.push_str(&format!("We decided on option {n}.\n"));
        }
        let cp = extract_checkpoint(&text, Vec::new());
        assert_eq!(cp.decisions.len(), MAX_DECISIONS);
        assert!(cp.decisions_truncated);
    }

    #[test]
    fn strips_ansi_before_matching() {
        let colored = "\x1b[32mWe decided to use rayon.\x1b[0m";
        let cp = extract_checkpoint(colored, Vec::new());
        assert_eq!(cp.decisions, vec!["We decided to use rayon.".to_string()]);
    }

    #[test]
    fn empty_input_yields_empty_checkpoint() {
        let cp = extract_checkpoint("", Vec::new());
        assert_eq!(cp.decisions, Vec::<String>::new());
        assert_eq!(cp.errors, Vec::<String>::new());
        assert_eq!(cp.files_changed, Vec::<String>::new());
        assert!(!cp.decisions_truncated);
        assert!(!cp.errors_truncated);
    }

    #[test]
    fn files_changed_dedup_passthrough() {
        let files = vec![
            "src/lib.rs".to_string(),
            "src/lib.rs".to_string(),
            "src/main.rs".to_string(),
        ];
        let cp = extract_checkpoint("", files);
        assert_eq!(
            cp.files_changed,
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );
    }

    #[test]
    fn files_changed_capped_at_max() {
        let files: Vec<String> = (0..(MAX_FILES + 25))
            .map(|n| format!("src/file_{n:04}.rs"))
            .collect();
        let cp = extract_checkpoint("", files);
        assert_eq!(cp.files_changed.len(), MAX_FILES);
        assert_eq!(cp.files_changed[0], "src/file_0000.rs");
    }
}
