//! Per-family compression handlers.
//!
//! Each `compress_*` takes already-ANSI-stripped text and returns a compact
//! summary. Handlers never need to worry about credentials or errors — the
//! orchestrator in [`super`] re-injects any preserved line a handler dropped,
//! and fails open before a handler ever runs on errored output. A handler that
//! cannot meaningfully shrink its input should return the input unchanged; the
//! orchestrator's 10%-savings gate then discards the no-op compression.

use std::sync::OnceLock;

use ahash::AHashMap;
use regex::Regex;

use super::detect::Family;

/// Dispatch to the handler for `family`.
pub fn compress(family: Family, text: &str) -> String {
    match family {
        Family::GitStatus => compress_git_status(text),
        Family::GitLog => compress_git_log(text),
        Family::GitDiff => compress_git_diff(text),
        Family::NpmInstall => compress_npm_install(text),
        Family::CargoBuild => compress_cargo_build(text),
        Family::Pytest => compress_pytest(text),
        Family::Ls => compress_ls(text),
        Family::Grep => compress_grep(text),
        Family::Logs => compress_logs(text),
    }
}

const GIT_DIFF_MIN_LINES: usize = 50;
const GIT_DIFF_KEEP_LINES: usize = 30;
const PYTEST_FAILURE_KEEP: usize = 30;
const LS_KEEP: usize = 50;
const GREP_MAX_PER_FILE: usize = 3;
const GREP_MAX_FILES: usize = 20;
const LOGS_MIN_LINES: usize = 10;

/// `git status` (long form) → one-line branch summary, or a per-section list of
/// changed file names when there are staged / unstaged / untracked entries.
fn compress_git_status(text: &str) -> String {
    let mut branch = "?".to_string();
    let mut ahead_behind = String::new();
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    let mut section: Option<&str> = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("On branch ") {
            branch = rest.trim().to_string();
        } else if line.contains("ahead") || line.contains("behind") {
            ahead_behind = line
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')')
                .to_string();
        } else if line.trim() == "nothing to commit, working tree clean"
            || line.trim() == "nothing to commit (working directory clean)"
            || line.trim() == "nothing to commit, working directory clean"
        {
            let suffix = if ahead_behind.is_empty() {
                String::new()
            } else {
                format!(" ({ahead_behind})")
            };
            return format!("branch: {branch}, clean{suffix}");
        } else if line.contains("Changes to be committed:") {
            section = Some("staged");
        } else if line.contains("Changes not staged") {
            section = Some("unstaged");
        } else if line.contains("Untracked files:") {
            section = Some("untracked");
        } else if (line.starts_with('\t') || line.starts_with("        ")) && section.is_some() {
            let mut fname = line.trim().to_string();
            for prefix in ["new file:", "modified:", "deleted:", "renamed:", "copied:"] {
                if let Some(stripped) = fname.strip_prefix(prefix) {
                    fname = stripped.trim().to_string();
                    break;
                }
            }
            match section {
                Some("staged") => staged.push(fname),
                Some("unstaged") => unstaged.push(fname),
                Some("untracked") => untracked.push(fname),
                _ => {}
            }
        }
    }

    let mut parts = vec![format!("branch: {branch}")];
    if !ahead_behind.is_empty() {
        parts.push(ahead_behind);
    }
    if !staged.is_empty() {
        parts.push(format!("{} staged: {}", staged.len(), staged.join(", ")));
    }
    if !unstaged.is_empty() {
        parts.push(format!(
            "{} unstaged: {}",
            unstaged.len(),
            unstaged.join(", ")
        ));
    }
    if !untracked.is_empty() {
        parts.push(format!(
            "{} untracked: {}",
            untracked.len(),
            untracked.join(", ")
        ));
    }
    if parts.len() > 2 {
        parts.join("\n")
    } else {
        parts.join(", ")
    }
}

/// `git log` → drop GPG signature and merge-noise lines, trim blanks.
fn compress_git_log(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.lines() {
        let s = line.trim();
        if s.is_empty()
            || s.starts_with("gpg:")
            || s.starts_with("Primary key")
            || s.starts_with("Merge:")
        {
            continue;
        }
        out.push(s.to_string());
    }
    if out.is_empty() {
        text.to_string()
    } else {
        out.join("\n")
    }
}

/// `git diff` → keep the first N lines plus an additions/deletions summary for
/// large diffs; small diffs pass through unchanged.
fn compress_git_diff(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= GIT_DIFF_MIN_LINES {
        return text.to_string();
    }
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in &lines {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
    }
    let mut out: Vec<String> = lines[..GIT_DIFF_KEEP_LINES]
        .iter()
        .map(|s| s.to_string())
        .collect();
    out.push(format!(
        "... ({} more lines, +{additions}/-{deletions} total)",
        lines.len() - GIT_DIFF_KEEP_LINES
    ));
    out.join("\n")
}

/// `npm install` / `pip install` → keep only the summary-bearing lines
/// (added / removed / audited / vulnerabilities / warnings / errors).
fn compress_npm_install(text: &str) -> String {
    const KEYWORDS: [&str; 13] = [
        "added",
        "removed",
        "changed",
        "audited",
        "packages",
        "vulnerability",
        "up to date",
        "successfully installed",
        "warn",
        "error",
        "fatal",
        // `npm ERR!` failure lines carry no `error:` colon — match the bang form.
        "err!",
        "npm err",
    ];
    let mut out = Vec::new();
    for line in text.lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let low = s.to_ascii_lowercase();
        if KEYWORDS.iter().any(|kw| low.contains(kw)) {
            out.push(s.to_string());
        }
    }
    if out.is_empty() {
        text.to_string()
    } else {
        out.join("\n")
    }
}

/// `cargo build` → keep warning / error lines and the final `Finished` /
/// `error[...]` summary; drop the bulk of `Compiling`/`Downloading` chatter.
fn compress_cargo_build(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 20 {
        return text.to_string();
    }
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    for line in &lines {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let low = s.to_ascii_lowercase();
        let noisy = low.starts_with("compiling ")
            || low.starts_with("downloading ")
            || low.starts_with("downloaded ")
            || low.starts_with("updating ")
            || low.starts_with("fetching ");
        let important = low.contains("error")
            || low.contains("warning")
            || s.starts_with("error[")
            || s.starts_with("warning:")
            || low.starts_with("finished ")
            || low.contains("could not compile");
        if important {
            kept.push(s.to_string());
        } else if noisy {
            dropped += 1;
        } else {
            kept.push(s.to_string());
        }
    }
    if dropped == 0 || kept.is_empty() {
        return text.to_string();
    }
    kept.join("\n")
}

/// Explicit runner summary markers (cargo, pytest short-summary, jest).
const TEST_SUMMARY_MARKERS: [&str; 4] = [
    "test result:",
    "short test summary",
    "tests:",
    "test suites:",
];

/// Matches a `<digits> <result-word>` count phrase with only whitespace between
/// the number and the word (`39 passed`, `2 failed`, `1 skipped`). The `\b`
/// before the digits prevents `test_0 passed` (underscore + 0) from matching,
/// which would otherwise pull every verbose per-test line into the summary.
fn summary_count_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|\s)\d+\s+(?:passed|passing|failed|failing|skipped|ignored|errors?)\b")
            .expect("summary count re compiles")
    })
}

/// Return `true` when `line` is a genuine test-run summary line — a numeric
/// count adjacent to a result word, or an explicit runner summary marker.
/// Per-test `PASSED`/`FAILED` status lines do NOT qualify.
fn is_test_summary_line(line: &str) -> bool {
    let low = line.to_ascii_lowercase();
    if TEST_SUMMARY_MARKERS.iter().any(|m| low.contains(m)) {
        return true;
    }
    summary_count_re().is_match(line)
}

/// pytest / cargo-test / generic runners → trailing summary block plus the
/// `FAILURES` / `failures:` section (first N lines) when present.
fn compress_pytest(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 15 {
        return text.to_string();
    }
    // Scan the tail for genuine summary lines. A summary line either pairs a
    // numeric count with a result word (`39 passed`, `1 skipped`, `2 failed`),
    // or is an explicit runner summary marker. Per-test `PASSED`/`FAILED`
    // status lines (no leading count) are intentionally NOT summary — counting
    // them would keep the whole verbose log and defeat compression.
    let tail_window = 60.min(lines.len());
    let mut summary: Vec<String> = Vec::new();
    for line in lines[lines.len() - tail_window..].iter() {
        let stripped = line.trim().trim_matches('=').trim();
        if stripped.is_empty() {
            continue;
        }
        if is_test_summary_line(stripped) {
            summary.push(stripped.to_string());
        }
    }

    // Pytest-native FAILURES / ERRORS section.
    let mut failures: Vec<&str> = Vec::new();
    let mut in_failures = false;
    for line in &lines {
        if line.contains("FAILURES") || line.contains("ERRORS") {
            in_failures = true;
            continue;
        }
        if in_failures {
            if line.starts_with("==========") {
                break;
            }
            failures.push(line);
        }
    }

    let summary_text = summary.join("\n");
    if !failures.is_empty() {
        let mut block: Vec<String> = failures
            .iter()
            .take(PYTEST_FAILURE_KEEP)
            .map(|s| s.to_string())
            .collect();
        if failures.len() > PYTEST_FAILURE_KEEP {
            block.push(format!(
                "... ({} more failure lines)",
                failures.len() - PYTEST_FAILURE_KEEP
            ));
        }
        return format!("{summary_text}\n\n{}", block.join("\n"));
    }
    if summary_text.is_empty() {
        text.to_string()
    } else {
        summary_text
    }
}

/// `ls` / `find` → head N entries plus a `(M more, T total)` marker.
fn compress_ls(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= LS_KEEP {
        return text.to_string();
    }
    let mut out: Vec<String> = lines[..LS_KEEP].iter().map(|s| s.to_string()).collect();
    out.push(format!(
        "... ({} more entries, {} total)",
        lines.len() - LS_KEEP,
        lines.len()
    ));
    out.join("\n")
}

/// `grep` / `rg` → group by file, keep top-N matches per file, cap file count.
fn compress_grep(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 10 {
        return text.to_string();
    }

    // Preserve first-seen file order while grouping.
    let mut order: Vec<String> = Vec::new();
    let mut groups: AHashMap<String, Vec<String>> = AHashMap::new();
    let mut no_file: Vec<String> = Vec::new();

    for line in &lines {
        if let Some(fname) = parse_grep_file(line) {
            groups
                .entry(fname.clone())
                .or_insert_with(|| {
                    order.push(fname.clone());
                    Vec::new()
                })
                .push((*line).to_string());
        } else if !line.trim().is_empty() {
            no_file.push((*line).to_string());
        }
    }

    if order.is_empty() {
        return text.to_string();
    }

    let total_matches: usize = groups.values().map(Vec::len).sum();
    let shown_files = order.len().min(GREP_MAX_FILES);
    let mut out = vec![format!(
        "[{total_matches} matches in {} files, showing top {shown_files}]",
        order.len()
    )];

    for (i, fname) in order.iter().enumerate() {
        if i >= GREP_MAX_FILES {
            out.push(format!(
                "... {} more files with matches omitted ...",
                order.len() - i
            ));
            break;
        }
        let file_lines = &groups[fname];
        if file_lines.len() <= GREP_MAX_PER_FILE {
            out.extend(file_lines.iter().cloned());
        } else {
            out.extend(file_lines[..GREP_MAX_PER_FILE].iter().cloned());
            out.push(format!(
                "  ... {} more matches in {fname} ...",
                file_lines.len() - GREP_MAX_PER_FILE
            ));
        }
    }

    if !no_file.is_empty() {
        out.push(String::new());
        out.extend(no_file.iter().take(10).cloned());
        if no_file.len() > 10 {
            out.push(format!(
                "... {} more non-file lines omitted ...",
                no_file.len() - 10
            ));
        }
    }

    out.join("\n")
}

/// Extract the leading `file` from a `file:line:content` grep line, returning
/// `None` when the line is not in that shape.
fn parse_grep_file(line: &str) -> Option<String> {
    // Find the first `:digits:` boundary using a non-greedy manual scan so a
    // colon inside the filename does not split early.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            // require at least one digit then another colon
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j] == b':' {
                return Some(line[..i].to_string());
            }
        }
        i += 1;
    }
    None
}

/// Generic logs → collapse runs of identical consecutive lines into
/// `<line>  (xN)`. Activates only when the duplicate rate is meaningful so
/// normal mixed logs pass through unchanged.
fn compress_logs(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < LOGS_MIN_LINES {
        return text.to_string();
    }
    let mut collapsed: Vec<String> = Vec::new();
    let mut dup_removed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let current = lines[i];
        let mut run = 1;
        while i + run < lines.len() && lines[i + run] == current {
            run += 1;
        }
        if run > 1 {
            collapsed.push(format!("{current}  (x{run})"));
            dup_removed += run - 1;
        } else {
            collapsed.push(current.to_string());
        }
        i += run;
    }
    // Require meaningful duplicate density before accepting the collapse.
    if dup_removed < (lines.len() / 3).max(LOGS_MIN_LINES) {
        return text.to_string();
    }
    collapsed.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_clean_one_line() {
        let out = compress_git_status("On branch main\nnothing to commit, working tree clean");
        assert_eq!(out, "branch: main, clean");
    }

    #[test]
    fn grep_file_parse() {
        assert_eq!(
            parse_grep_file("src/a.rs:10:foo"),
            Some("src/a.rs".to_string())
        );
        assert_eq!(
            parse_grep_file("C:/win/path.rs:3:x"),
            Some("C:/win/path.rs".to_string())
        );
        assert_eq!(parse_grep_file("no colon digit here"), None);
    }

    #[test]
    fn logs_collapse_duplicates() {
        let mut input = String::new();
        for _ in 0..30 {
            input.push_str("repeated line\n");
        }
        let out = compress_logs(&input);
        assert!(out.contains("(x30)"), "expected run marker, got: {out}");
    }
}
