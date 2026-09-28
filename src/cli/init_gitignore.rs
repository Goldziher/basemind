//! `.gitignore` coverage for the `.local` files `basemind init` writes.
//!
//! `basemind init` writes several files that are gitignored BY CONVENTION rather than by any
//! mechanism basemind controls (`CLAUDE.local.md`, `AGENTS.local.md`, `.claude/settings.local.json`,
//! the `.ai-rulez/local/` rule tree). If the host repo's `.gitignore` doesn't actually cover one of
//! these, a later `git add -A` commits a file the user believed was personal/machine-local. This
//! module closes that gap: given a target path, it checks whether any `.gitignore` between the repo
//! root and the target's directory already covers it (the same precedence the `ignore` crate itself
//! applies — a deeper `.gitignore` can re-include what a parent excludes), and if not, offers to
//! append a pattern to the repo-root `.gitignore`.
//!
//! Deliberately reuses the `ignore` crate's own gitignore engine ([`ignore::gitignore`]) rather than
//! hand-rolling glob matching — the same crate [`super::super::scanner_filter`] uses for the scan
//! walk, so "is this covered by `.gitignore`" means the same thing everywhere in the codebase.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::gitignore::GitignoreBuilder;

/// Given a target file `basemind init` might write, the `.gitignore` pattern it should be covered
/// by, or `None` when `target` is a COMMITTED (shared) file that must never be gitignored — e.g.
/// `CLAUDE.md` or the committed ai-rulez rule file. Only the known `.local`-flavored targets get a
/// pattern; anything else is left alone.
pub(crate) fn local_pattern(root: &Path, target: &Path) -> Option<String> {
    let rel = target.strip_prefix(root).ok()?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    match rel.as_str() {
        "CLAUDE.local.md" | "AGENTS.local.md" | ".claude/settings.local.json" => Some(rel),
        // ~keep The whole `.ai-rulez/local/` tree, not just today's file — any future basemind-owned
        // ~keep file dropped under it should stay covered without a new pattern per file.
        _ if rel.starts_with(".ai-rulez/local/") => Some(".ai-rulez/local/".to_string()),
        _ => None,
    }
}

/// True when `target` (an absolute path under `root`) is already excluded by some `.gitignore`
/// between `root` and `target`'s parent directory. Does not require `target` to exist on disk —
/// gitignore matching is purely path-based.
pub(crate) fn is_covered(root: &Path, target: &Path) -> Result<bool> {
    let mut builder = GitignoreBuilder::new(root);
    for dir in ancestor_dirs(root, target) {
        let candidate = dir.join(".gitignore");
        if candidate.is_file()
            && let Some(err) = builder.add(&candidate)
        {
            return Err(anyhow::Error::new(err).context(format!("parse {}", candidate.display())));
        }
    }
    let matcher = builder.build().context("compile gitignore matcher")?;
    Ok(matcher.matched(target, false).is_ignore())
}

/// Ensure `target` is covered by `.gitignore`, prompting in an interactive TTY unless `assume_yes`.
/// Returns the `.gitignore` path that was appended to, or `None` if nothing changed (already
/// covered, or the user declined the prompt).
pub(crate) fn ensure_coverage(root: &Path, target: &Path, pattern: &str, assume_yes: bool) -> Result<Option<PathBuf>> {
    if is_covered(root, target)? {
        return Ok(None);
    }
    let should_add = assume_yes || !std::io::stdin().is_terminal() || prompt_add_to_gitignore(pattern)?;
    if !should_add {
        return Ok(None);
    }
    let gitignore_path = root.join(".gitignore");
    append_pattern(&gitignore_path, pattern)?;
    Ok(Some(gitignore_path))
}

/// Hand-rolled Y/n prompt over stdin, matching the style of the other `init.rs` prompts. A blank
/// answer accepts the default (yes) — appending a gitignore pattern for a `.local` file is safe
/// hygiene, not a consequential choice.
fn prompt_add_to_gitignore(pattern: &str) -> Result<bool> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    write!(stdout, "add {pattern:?} to .gitignore? [Y/n] ").context("write prompt")?;
    stdout.flush().context("flush prompt")?;
    let mut line = String::new();
    stdin.read_line(&mut line).context("read stdin")?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Append `pattern` to `gitignore_path` with a trailing newline, creating the file if absent and
/// skipping the append entirely if the exact pattern is already on its own line. The rest of the
/// file — content and order — is preserved verbatim.
fn append_pattern(gitignore_path: &Path, pattern: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(gitignore_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("read {}", gitignore_path.display()))),
    };
    if existing.lines().any(|l| l.trim() == pattern) {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(pattern);
    out.push('\n');
    std::fs::write(gitignore_path, out).with_context(|| format!("write {}", gitignore_path.display()))
}

/// Directories from `root` down to `target`'s parent, inclusive, in root-to-leaf order — the order
/// `.gitignore` files must be added to a [`GitignoreBuilder`] in, so a deeper, more specific
/// `.gitignore` is layered on top of (and can override) a shallower one, matching real git
/// semantics. `target` need not exist; this is purely a path computation.
fn ancestor_dirs(root: &Path, target: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let Ok(rel) = target.strip_prefix(root) else {
        return dirs;
    };
    let mut cur = root.to_path_buf();
    if let Some(parent) = rel.parent() {
        for comp in parent.components() {
            cur = cur.join(comp);
            dirs.push(cur.clone());
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_pattern_covers_known_local_targets_and_ignores_committed_ones() {
        let root = Path::new("/repo");
        assert_eq!(
            local_pattern(root, &root.join("CLAUDE.local.md")),
            Some("CLAUDE.local.md".to_string())
        );
        assert_eq!(
            local_pattern(root, &root.join("AGENTS.local.md")),
            Some("AGENTS.local.md".to_string())
        );
        assert_eq!(
            local_pattern(root, &root.join(".claude").join("settings.local.json")),
            Some(".claude/settings.local.json".to_string())
        );
        assert_eq!(
            local_pattern(
                root,
                &root.join(".ai-rulez").join("local").join("rules").join("basemind-usage.md")
            ),
            Some(".ai-rulez/local/".to_string())
        );
        assert_eq!(local_pattern(root, &root.join("CLAUDE.md")), None, "committed file untouched");
        assert_eq!(
            local_pattern(root, &root.join(".ai-rulez").join("rules").join("basemind-usage.md")),
            None,
            "committed ai-rulez rule untouched"
        );
    }

    #[test]
    fn ensure_coverage_appends_a_missing_pattern_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let target = root.join("CLAUDE.local.md");

        assert!(!is_covered(root, &target).expect("check coverage"), "nothing covers it yet");

        let appended = ensure_coverage(root, &target, "CLAUDE.local.md", true)
            .expect("ensure coverage")
            .expect("a pattern was appended");
        assert_eq!(appended, root.join(".gitignore"));
        let contents = std::fs::read_to_string(root.join(".gitignore")).expect("read .gitignore");
        assert_eq!(contents, "CLAUDE.local.md\n");
        assert!(is_covered(root, &target).expect("check coverage"), "now covered");

        // ~keep Re-running must be a no-op: no duplicate line, no error, nothing appended.
        let second = ensure_coverage(root, &target, "CLAUDE.local.md", true).expect("second run");
        assert!(second.is_none(), "already covered — nothing to append");
        let contents_after = std::fs::read_to_string(root.join(".gitignore")).expect("read .gitignore");
        assert_eq!(contents_after, contents, "no duplicate pattern written");
    }

    #[test]
    fn ensure_coverage_is_a_no_op_when_already_covered_by_an_existing_gitignore() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join(".gitignore"), "# comment\nCLAUDE.local.md\nother.txt\n").expect("seed .gitignore");
        let target = root.join("CLAUDE.local.md");

        assert!(is_covered(root, &target).expect("check coverage"));
        let result = ensure_coverage(root, &target, "CLAUDE.local.md", true).expect("ensure coverage");
        assert!(result.is_none(), "already covered — nothing to append");
        let contents = std::fs::read_to_string(root.join(".gitignore")).expect("read .gitignore");
        assert_eq!(
            contents, "# comment\nCLAUDE.local.md\nother.txt\n",
            "existing file preserved verbatim"
        );
    }

    #[test]
    fn ensure_coverage_respects_a_glob_pattern_in_a_nested_gitignore() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".claude")).expect("mkdir .claude");
        std::fs::write(root.join(".claude").join(".gitignore"), "settings.local.json\n").expect("seed nested gitignore");
        let target = root.join(".claude").join("settings.local.json");

        assert!(
            is_covered(root, &target).expect("check coverage"),
            "a nested .gitignore must be honored, not just the root one"
        );
        let result = ensure_coverage(root, &target, ".claude/settings.local.json", true).expect("ensure coverage");
        assert!(result.is_none(), "already covered by the nested file");
        assert!(!root.join(".gitignore").exists(), "no root .gitignore should be created");
    }
}
