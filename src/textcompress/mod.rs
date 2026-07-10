//! Behavioral output compression (token-reduction workstream W6).
//!
//! Compress verbose command output into a compact summary so an agent's tool
//! results cost far fewer tokens — while NEVER dropping errors or secrets.
//!
//! The single public entry point is [`compress_output`]. Given raw command
//! output (and an optional family hint), it:
//!
//! 1. Strips ANSI escape codes (keeping visible text).
//! 2. Fails open — returns the raw input unchanged with `compressed = false` —
//!    when the output shows the command errored, or when no family handler can
//!    save at least [`MIN_SAVINGS_RATIO`].
//! 3. Otherwise runs the family handler, then re-injects any credential- or
//!    error-bearing line a handler would have dropped, so secrets and error
//!    detail always survive.
//!
//! Ported from the alexgreensh/token-optimizer `bash_compress.py` reference,
//! adapted to a pure read-only `text -> text` transform (this tool never runs
//! the command — it only sees output).

pub mod checkpoint;
pub mod cli;
pub mod delta;
mod detect;
mod handlers;
mod safety;
pub mod waste;

pub use detect::Family;

/// Result of a [`compress_output`] call.
///
/// `output` is the text to surface to the agent: the compact summary when
/// `compressed` is `true`, or the (ANSI-stripped) raw input when it is `false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionOutcome {
    /// The text to emit downstream.
    pub output: String,
    /// Byte length of the original input (after ANSI stripping).
    pub original_bytes: usize,
    /// Byte length of [`output`](Self::output).
    pub compressed_bytes: usize,
    /// The family that was used (detected or supplied), as a stable lowercase string.
    pub family_detected: String,
    /// Whether compression was applied. `false` means the raw input was returned.
    pub compressed: bool,
}

/// Minimum byte-savings ratio required to accept a compression. Below this we
/// fail open and return the raw input — the risk of dropping a meaningful line
/// is not worth a marginal token win.
pub const MIN_SAVINGS_RATIO: f64 = 0.10;

/// Inputs below this byte length are never worth compressing; pass through.
const MIN_INPUT_BYTES: usize = 100;

/// Cap on re-injected preserved lines, so output where most lines carry a
/// secret (e.g. grep for keys) does not defeat compression entirely.
const MAX_REINJECTED: usize = 32;

/// Compress `text` for a command `family`. When `family` is `None`, the family
/// is sniffed from the output shape (see `detect`).
///
/// Fail-open guarantees (the security-critical contract):
/// - errored output (error / fatal / exception / traceback markers) is returned raw;
/// - any line carrying a credential or error marker is preserved verbatim;
/// - a compression saving less than [`MIN_SAVINGS_RATIO`] is discarded.
pub fn compress_output(text: &str, family: Option<&str>) -> CompressionOutcome {
    let cleaned = safety::strip_ansi(text);
    let original_bytes = cleaned.len();

    let resolved = match family {
        Some(name) => detect::Family::parse(name).unwrap_or_else(|| detect::detect(&cleaned)),
        None => detect::detect(&cleaned),
    };
    let family_name = resolved.as_str().to_string();

    let pass_through = |out: String| {
        let len = out.len();
        CompressionOutcome {
            output: out,
            original_bytes,
            compressed_bytes: len,
            family_detected: family_name.clone(),
            compressed: false,
        }
    };

    if safety::contains_credential(text) && !safety::contains_credential(&cleaned) {
        return pass_through(text.to_string());
    }

    if original_bytes < MIN_INPUT_BYTES {
        return pass_through(cleaned);
    }

    if safety::looks_like_failure(&cleaned) {
        return pass_through(cleaned);
    }

    let mut compressed = handlers::compress(resolved, &cleaned);

    let preserved = safety::preserved_line_indices(&cleaned);
    if !preserved.is_empty() {
        match reinject_preserved(&cleaned, &compressed, &preserved) {
            Reinjection::Complete(out) => compressed = out,
            Reinjection::Overflowed => return pass_through(cleaned),
        }
    }

    if safety::contains_credential(&cleaned) && !safety::contains_credential(&compressed) {
        return pass_through(cleaned);
    }

    let compressed_bytes = compressed.len();
    let saved = if original_bytes == 0 {
        0.0
    } else {
        1.0 - (compressed_bytes as f64 / original_bytes as f64)
    };
    if saved < MIN_SAVINGS_RATIO {
        return pass_through(cleaned);
    }

    CompressionOutcome {
        output: compressed,
        original_bytes,
        compressed_bytes,
        family_detected: family_name,
        compressed: true,
    }
}

/// Outcome of re-injecting preserved lines into a handler's output.
enum Reinjection {
    /// Every preserved line not already present was appended (within the cap).
    Complete(String),
    /// More droppable preserved lines exist than [`MAX_REINJECTED`] allows;
    /// the caller must fail open rather than truncate and lose secrets.
    Overflowed,
}

/// Append any preserved line (by original index) that is not already present as
/// an exact line in `compressed`. If the number of lines that would need
/// appending exceeds [`MAX_REINJECTED`], return [`Reinjection::Overflowed`] so
/// the caller fails open — truncating preserved lines could silently drop a
/// secret while still reporting a successful compression.
fn reinject_preserved(cleaned: &str, compressed: &str, preserved: &ahash::AHashSet<usize>) -> Reinjection {
    let original_lines: Vec<&str> = cleaned.lines().collect();
    let mut existing: ahash::AHashSet<&str> = compressed.lines().collect();

    let mut sorted: Vec<usize> = preserved.iter().copied().collect();
    sorted.sort_unstable();

    let mut appended: Vec<String> = Vec::new();
    for idx in sorted {
        if let Some(line) = original_lines.get(idx)
            && !existing.contains(line)
        {
            if appended.len() >= MAX_REINJECTED {
                return Reinjection::Overflowed;
            }
            appended.push((*line).to_string());
            existing.insert(line);
        }
    }

    if appended.is_empty() {
        return Reinjection::Complete(compressed.to_string());
    }
    Reinjection::Complete(format!("{compressed}\n{}", appended.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_shrinks() {
        let input = "On branch main\nYour branch is up to date with 'origin/main'.\n\n\
                     Changes not staged for commit:\n  (use \"git add ...\")\n\
                     \tmodified:   src/lib.rs\n\tmodified:   src/main.rs\n\
                     \tmodified:   Cargo.toml\n\nUntracked files:\n  (use \"git add ...\")\n\
                     \tnewfile.rs\n\tother.rs\n\nno changes added to commit\n";
        let r = compress_output(input, Some("git_status"));
        assert!(r.compressed, "git status should compress: {:?}", r.output);
        assert!(r.compressed_bytes < r.original_bytes);
        assert!(r.output.contains("branch: main"));
        assert!(r.output.contains("unstaged"));
    }

    #[test]
    fn git_log_shrinks() {
        let mut input = String::new();
        for n in 0..30 {
            input.push_str(&format!(
                "commit {:040x}\ngpg: Signature made ...\ngpg: Good signature\n\
                 Author: Dev <d@x>\nDate:   today\n\n    message {n}\n\n",
                n
            ));
        }
        let r = compress_output(&input, Some("git_log"));
        assert!(r.compressed, "git log should compress");
        assert!(!r.output.contains("gpg:"), "gpg lines should be dropped");
    }

    #[test]
    fn git_diff_shrinks() {
        let mut input = String::from("diff --git a/x b/x\n--- a/x\n+++ b/x\n");
        for n in 0..200 {
            input.push_str(&format!("+added line {n}\n"));
        }
        let r = compress_output(&input, Some("git_diff"));
        assert!(r.compressed, "large diff should compress");
        assert!(r.output.contains("more lines"));
    }

    #[test]
    fn npm_install_shrinks() {
        let mut input = String::new();
        for n in 0..60 {
            input.push_str(&format!("npm http fetch GET 200 https://registry/pkg{n} 12ms\n"));
        }
        input.push_str("added 60 packages, and audited 61 packages in 3s\n");
        input.push_str("found 0 vulnerabilities\n");
        let r = compress_output(&input, Some("npm_install"));
        assert!(r.compressed, "npm install should compress");
        assert!(r.output.contains("audited"));
        assert!(!r.output.contains("npm http fetch"));
    }

    #[test]
    fn cargo_build_shrinks() {
        let mut input = String::new();
        for n in 0..60 {
            input.push_str(&format!("   Compiling crate{n} v0.1.0\n"));
        }
        input.push_str("warning: unused variable: `x`\n");
        input.push_str("    Finished dev [unoptimized] target(s) in 12.3s\n");
        let r = compress_output(&input, Some("cargo_build"));
        assert!(r.compressed, "cargo build should compress");
        assert!(r.output.contains("warning"));
        assert!(r.output.contains("Finished"));
    }

    #[test]
    fn pytest_shrinks() {
        let mut input = String::from("============ test session starts ============\n");
        for n in 0..40 {
            input.push_str(&format!(
                "tests/test_mod.py::test_{n} PASSED                  [ {n}%]\n"
            ));
        }
        input.push_str("===================== 39 passed, 1 skipped in 2.10s =====================\n");
        let r = compress_output(&input, Some("pytest"));
        assert!(r.compressed, "pytest should compress: {:?}", r.output);
        assert!(r.output.contains("passed"));
    }

    #[test]
    fn ls_shrinks() {
        let mut input = String::new();
        for n in 0..120 {
            input.push_str(&format!("file_{n:04}.txt\n"));
        }
        let r = compress_output(&input, Some("ls"));
        assert!(r.compressed, "long listing should compress");
        assert!(r.output.contains("more entries"));
    }

    #[test]
    fn grep_shrinks() {
        let mut input = String::new();
        for f in 0..30 {
            for l in 0..10 {
                input.push_str(&format!("src/file{f}.rs:{}:    let value = compute();\n", l + 1));
            }
        }
        let r = compress_output(&input, Some("grep"));
        assert!(r.compressed, "grep output should compress");
        assert!(r.output.contains("matches in"));
        assert!(r.output.contains("more matches in"));
    }

    #[test]
    fn logs_shrinks() {
        let mut input = String::new();
        for _ in 0..50 {
            input.push_str("waiting for connection...\n");
        }
        let r = compress_output(&input, Some("logs"));
        assert!(r.compressed, "duplicate-heavy logs should compress");
        assert!(r.output.contains("(x50)"));
    }

    #[test]
    fn fail_open_on_errored_output() {
        let mut input = String::from("On branch main\n");
        for n in 0..40 {
            input.push_str(&format!("\tmodified:   file{n}.rs\n"));
        }
        input.push_str("error: something went catastrophically wrong\n");
        let r = compress_output(&input, Some("git_status"));
        assert!(!r.compressed, "errored output must not compress");
        assert!(r.output.contains("error: something went catastrophically wrong"));
        assert_eq!(r.output, safety_strip(&input), "raw passthrough expected");
    }

    #[test]
    fn fail_open_on_low_savings() {
        let mut input = String::new();
        for n in 0..40 {
            input.push_str(&format!("unique log line number {n} with distinct content here\n"));
        }
        let r = compress_output(&input, Some("logs"));
        assert!(!r.compressed, "no-savings output must pass through raw");
        assert_eq!(r.output, input.trim_end_matches('\n').to_string() + "\n");
    }

    #[test]
    fn preserves_aws_key_in_droppable_git_status() {
        let mut input = String::from("On branch main\n\nUntracked files:\n  (use \"git add\")\n");
        for n in 0..40 {
            input.push_str(&format!("\tjunk_file_{n}.tmp\n"));
        }
        input.push_str("\tleaked_AKIAIOSFODNN7EXAMPLE_creds.txt\n");
        let r = compress_output(&input, Some("git_status"));
        assert!(
            r.output.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key must survive compression, got: {}",
            r.output
        );
    }

    #[test]
    fn preserves_github_pat_in_logs() {
        let pat = format!("ghp_{}", "a".repeat(36));
        let mut input = String::new();
        for _ in 0..50 {
            input.push_str("connecting...\n");
        }
        input.push_str(&format!("auth header token {pat}\n"));
        let r = compress_output(&input, Some("logs"));
        assert!(r.output.contains(&pat), "GitHub PAT must survive: {}", r.output);
    }

    #[test]
    fn preserves_private_key_header() {
        let mut input = String::new();
        for n in 0..60 {
            input.push_str(&format!("file_{n}.pem\n"));
        }
        input.push_str("-----BEGIN RSA PRIVATE KEY-----\n");
        let r = compress_output(&input, Some("ls"));
        assert!(
            r.output.contains("-----BEGIN RSA PRIVATE KEY-----"),
            "private key header must survive: {}",
            r.output
        );
    }

    #[test]
    fn preserves_secret_in_osc8_hyperlink_uri() {
        let secret_link = "\x1b]8;;postgres://admin:SECRETPASSWORD@db.internal/prod\x07ok\x1b]8;;\x07";
        let mut input = String::new();
        for _ in 0..30 {
            input.push_str("waiting for connection...\n");
        }
        input.push_str(secret_link);
        input.push('\n');

        let r = compress_output(&input, Some("logs"));
        assert!(!r.compressed, "OSC8-URI secret must fail open, not compress");
        assert_eq!(r.output, input, "must return the RAW pre-strip input");
        assert!(
            r.output.contains("postgres://admin:SECRETPASSWORD@db.internal/prod"),
            "credential in the hyperlink URI must survive: {}",
            r.output
        );
    }

    #[test]
    fn npm_err_failures_fail_open_raw() {
        let mut input = String::new();
        for n in 0..10 {
            input.push_str(&format!("npm ERR! line {n} install failed for dep{n}\n"));
        }
        let r = compress_output(&input, Some("npm_install"));
        assert!(!r.compressed, "npm ERR! output must fail open, not compress");
        assert_eq!(r.output, input, "raw passthrough expected for npm ERR! block");
        for n in 0..10 {
            assert!(
                r.output.contains(&format!("npm ERR! line {n}")),
                "npm ERR! line {n} must survive: {}",
                r.output
            );
        }
    }

    #[test]
    fn fail_open_when_preserved_lines_exceed_cap() {
        let count = MAX_REINJECTED + 5;
        let mut input = String::new();
        for n in 0..80 {
            input.push_str(&format!("plain_file_{n:04}.txt\n"));
        }
        for n in 0..count {
            input.push_str(&format!("file_AKIA{:016X}_creds.txt\n", n));
        }
        let r = compress_output(&input, Some("ls"));
        assert!(
            !r.compressed,
            "more than {MAX_REINJECTED} secret lines must fail open, not truncate"
        );
        for n in 0..count {
            assert!(
                r.output.contains(&format!("AKIA{:016X}", n)),
                "secret #{n} must survive in raw output"
            );
        }
        assert!(
            !r.output.contains("preserved in raw output"),
            "misleading cap note must be removed: {}",
            r.output
        );
    }

    fn safety_strip(s: &str) -> String {
        super::safety::strip_ansi(s)
    }
}
