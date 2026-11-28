//! Document-scope detection for `basemind init`.
//!
//! The document tier (RAG) indexes every non-code file that passes `[scan]`, minus a default
//! deny-list. On a large repository most of that is noise, so init counts the document-like files
//! by extension, applies the default scope, proposes extra exclusions for scratch-like directories
//! and writes the result into `[documents]` as plain glob lists: nothing is hidden in code.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::default_document_exclude;
use crate::config::rules::{RuleSet, compile_rules};

/// Extensions init counts: documentation, Office, markup, data and raster images.
pub(crate) const COUNTED_EXTENSIONS: &[&str] = &[
    "md", "mdx", "rst", "adoc", "txt", "ipynb", "pdf", "doc", "docx", "pptx", "xlsx", "odt", "html", "htm", "csv",
    "tsv", "xml", "json", "yaml", "yml", "toml", "png", "jpg", "jpeg", "gif", "webp", "tiff", "bmp",
];

/// What looks like documentation: the extensions written into `[documents] include`.
const DOCUMENTATION_EXTENSIONS: &[&str] = &[
    "md", "mdx", "rst", "adoc", "txt", "ipynb", "pdf", "doc", "docx", "pptx", "xlsx", "odt", "html", "htm",
];

/// Directory names that usually hold throwaway or superseded material.
const SCRATCH_DIRS: &[&str] = &[
    "playground",
    "sandbox",
    "scratch",
    "tmp",
    "experiments",
    "legacy",
    "archive",
    "deprecated",
    "backup",
];

/// A scratch directory with at least this many in-scope documents is excluded under `--yes`; a
/// smaller one is only printed as a commented suggestion.
const HIGH_CONFIDENCE_MIN_DOCS: usize = 10;

/// Directories the walk never enters.
const SKIPPED_DIRS: &[&str] = &[".git", ".basemind"];

/// Document-like files of one extension.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExtCount {
    pub total: usize,
    /// Files that stay in scope under the proposed `include` and the default `exclude`.
    pub kept: usize,
}

/// A scratch-like directory whose documents are in scope by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScratchDir {
    /// Repo-relative directory, forward-slash.
    pub dir: String,
    pub files: usize,
    pub bytes: u64,
}

impl ScratchDir {
    /// The glob that excludes the directory and everything beneath it.
    fn glob(&self) -> String {
        format!("{}/**", escape_glob(&self.dir))
    }

    fn high_confidence(&self) -> bool {
        self.files >= HIGH_CONFIDENCE_MIN_DOCS
    }
}

/// `dir` as a literal glob: metacharacters become single-character classes, and a leading `!` is
/// shielded with `./` (stripped when the glob expands) so the entry is not read as a re-include.
pub(crate) fn escape_glob(dir: &str) -> String {
    let mut out = String::with_capacity(dir.len() + 2);
    if dir.starts_with('!') {
        out.push_str("./");
    }
    for c in dir.chars() {
        match c {
            '*' | '?' | '[' | '{' | '}' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

/// What init found in the repository.
#[derive(Debug, Default)]
pub(crate) struct ScopeReport {
    pub by_ext: BTreeMap<String, ExtCount>,
    pub scratch: Vec<ScratchDir>,
}

impl ScopeReport {
    pub(crate) fn totals(&self) -> ExtCount {
        self.by_ext.values().fold(ExtCount::default(), |acc, c| ExtCount {
            total: acc.total + c.total,
            kept: acc.kept + c.kept,
        })
    }
}

/// How scratch-directory suggestions are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Ask keep / exclude / skip for each suggestion.
    Interactive,
    /// Exclude the high-confidence suggestions, print the rest commented.
    Yes,
    /// Write nothing extra: every suggestion is printed commented.
    Preview,
}

/// The proposed `[documents]` lists.
pub(crate) fn proposed_include() -> Vec<String> {
    DOCUMENTATION_EXTENSIONS.iter().map(|e| format!("**/*.{e}")).collect()
}

/// The documents the proposed `[documents]` lists keep: the include list, minus the default
/// exclude list and any extra exclusions.
pub(crate) struct DocumentScope {
    include: RuleSet,
    exclude: RuleSet,
}

impl DocumentScope {
    /// The default scope with `excluded` globs (the scratch directories already dropped) removed too.
    pub(crate) fn new(excluded: &[String]) -> Result<Self> {
        let include = proposed_include();
        let exclude: Vec<String> = default_document_exclude()
            .into_iter()
            .chain(excluded.iter().cloned())
            .collect();
        Ok(Self {
            include: compile_rules(include.iter().map(String::as_str)).map_err(anyhow::Error::msg)?,
            exclude: compile_rules(exclude.iter().map(String::as_str)).map_err(anyhow::Error::msg)?,
        })
    }

    /// True when the repo-relative, forward-slash `rel` stays in scope.
    pub(crate) fn keeps(&self, rel: &str) -> bool {
        self.include.is_match(rel) && !self.exclude.is_match(rel)
    }
}

/// Walk `root` (honouring `.gitignore`) and count document-like files against the default scope.
pub(crate) fn detect(root: &Path) -> Result<ScopeReport> {
    let scope = DocumentScope::new(&[])?;
    let mut report = ScopeReport::default();
    let mut scratch: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let walker = crate::scanner_filter::ignore_walk_builder(root, true, false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|t| t.is_dir())
                || !SKIPPED_DIRS.iter().any(|d| entry.file_name() == *d)
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let Some(ext) = rel.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) else {
            continue;
        };
        if !COUNTED_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        let counts = report.by_ext.entry(ext).or_default();
        counts.total += 1;
        if !scope.keeps(&rel) {
            continue;
        }
        counts.kept += 1;
        if let Some(dir) = scratch_ancestor(&rel) {
            let slot = scratch.entry(dir).or_default();
            slot.0 += 1;
            slot.1 += entry.metadata().map_or(0, |m| m.len());
        }
    }
    report.scratch = scratch
        .into_iter()
        .map(|(dir, (files, bytes))| ScratchDir { dir, files, bytes })
        .collect();
    Ok(report)
}

/// The outermost directory of `rel` whose name is scratch-like, as a repo-relative path.
fn scratch_ancestor(rel: &str) -> Option<String> {
    let segments: Vec<&str> = rel.split('/').collect();
    let dirs = segments.split_last()?.1;
    let at = dirs
        .iter()
        .position(|segment| SCRATCH_DIRS.contains(&segment.to_ascii_lowercase().as_str()))?;
    Some(dirs[..=at].join("/"))
}

/// Resolve the suggestions into `(excluded globs, commented suggestions)`.
pub(crate) fn resolve(
    report: &ScopeReport,
    mode: Mode,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut excluded = Vec::new();
    let mut commented = Vec::new();
    for dir in &report.scratch {
        let describe = format!("{}/ ({} documents, {})", dir.dir, dir.files, human_bytes(dir.bytes));
        let exclude = match mode {
            Mode::Yes => dir.high_confidence(),
            Mode::Preview => false,
            Mode::Interactive => match ask(&describe, input, output)? {
                Answer::Exclude => true,
                Answer::Keep => continue,
                Answer::Skip => false,
            },
        };
        if exclude {
            excluded.push(dir.glob());
        } else {
            commented.push(format!("{}  # {describe}", dir.glob()));
        }
    }
    Ok((excluded, commented))
}

enum Answer {
    Exclude,
    Keep,
    Skip,
}

fn ask(describe: &str, input: &mut dyn BufRead, output: &mut dyn Write) -> Result<Answer> {
    write!(
        output,
        "exclude {describe} from documents? [e]xclude / [k]eep / [s]kip (default skip): "
    )
    .context("write prompt")?;
    output.flush().context("flush prompt")?;
    let mut line = String::new();
    input.read_line(&mut line).context("read answer")?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "e" | "exclude" | "y" | "yes" => Answer::Exclude,
        "k" | "keep" | "n" | "no" => Answer::Keep,
        _ => Answer::Skip,
    })
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{} KiB", bytes.div_ceil(KIB))
    }
}

/// Documents still in scope once the `excluded` globs drop their scratch directories.
pub(crate) fn kept_after_exclusions(report: &ScopeReport, excluded: &[String]) -> usize {
    let dropped: usize = report
        .scratch
        .iter()
        .filter(|dir| excluded.contains(&dir.glob()))
        .map(|dir| dir.files)
        .sum();
    report.totals().kept.saturating_sub(dropped)
}

/// The counts init prints before it writes: documents per extension, and in scope before and
/// after the proposed scope (`kept`), plus the exclusions chosen.
pub(crate) fn render_counts(report: &ScopeReport, excluded: &[String]) -> String {
    let totals = report.totals();
    let mut out = String::from("documents: extension  found -> in scope\n");
    for (ext, c) in &report.by_ext {
        out.push_str(&format!("  .{ext:<6} {:>7} -> {}\n", c.total, c.kept));
    }
    out.push_str(&format!(
        "  total   {:>7} -> {} (default scope: tests, agent config, vendored output and data files dropped)",
        totals.total, totals.kept
    ));
    for glob in excluded {
        out.push_str(&format!("\n  also excluded: {glob}"));
    }
    if !excluded.is_empty() {
        out.push_str(&format!(
            "\n  in scope after exclusions: {}",
            kept_after_exclusions(report, excluded)
        ));
    }
    out
}

/// The `include` / `exclude` lines of the `[documents]` block, every list written out.
pub(crate) fn render_lists(excluded: &[String], commented: &[String]) -> String {
    let list = |name: &str, items: &[String]| {
        let body: String = items.iter().map(|g| format!("    {g:?},\n")).collect();
        format!("{name} = [\n{body}]\n")
    };
    let mut exclude = default_document_exclude();
    exclude.extend(excluded.iter().cloned());
    let mut out = String::from(
        "# Written by `basemind init`; edit freely. `include` keeps what looks like documentation; `exclude`\n\
         # drops tests, agent config, vendored output and the paths below. A later `!pattern` entry\n\
         # re-includes what an earlier one dropped, e.g. exclude = [..., \"!tests/docs/**\"].\n",
    );
    out.push_str(&list("include", &proposed_include()));
    out.push_str(&list("exclude", &exclude));
    if !commented.is_empty() {
        out.push_str("# Suggested, not applied; add any of these to `exclude` to drop it:\n");
        for line in commented.iter().flat_map(|suggestion| suggestion.lines()) {
            out.push_str(&format!("#   {line}\n"));
        }
    }
    out
}

/// The scaffold lines the rendered lists replace.
const SCAFFOLD_PLACEHOLDER: &str = "# include = []\n# exclude = []\n";

/// `scaffold` with its commented `[documents] include` / `exclude` placeholders replaced by `lists`.
pub(crate) fn apply_to_scaffold(scaffold: &str, lists: &str) -> String {
    scaffold.replacen(SCAFFOLD_PLACEHOLDER, lists, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, rel: &str, bytes: usize) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        touch(root, "README.md", 10);
        touch(root, "docs/guide.md", 10);
        touch(root, "docs/spec.pdf", 10);
        touch(root, "tests/fixtures/page.html", 10);
        touch(root, "tests/report.pdf", 10);
        touch(root, "data/rows.csv", 10);
        touch(root, "CLAUDE.md", 10);
        touch(root, "ignored/secret.pdf", 10);
        for i in 0..HIGH_CONFIDENCE_MIN_DOCS {
            touch(root, &format!("legacy/old/{i}.md"), 2048);
        }
        touch(root, "sandbox/notes.md", 10);
        tmp
    }

    #[test]
    fn scratch_glob_matches_the_literal_directory_whatever_its_name() {
        for (dir, inside, outside) in [
            ("legacy", "legacy/a.md", "other/a.md"),
            ("docs/[draft]/legacy", "docs/[draft]/legacy/a.md", "docs/d/legacy/a.md"),
            ("docs/[draft/legacy", "docs/[draft/legacy/a.md", "docs/d/legacy/a.md"),
            ("!old/legacy", "!old/legacy/a.md", "old/legacy/a.md"),
            ("a*b/legacy", "a*b/legacy/a.md", "axb/legacy/a.md"),
            ("a?b/legacy", "a?b/legacy/a.md", "axb/legacy/a.md"),
            ("{x,y}/legacy", "{x,y}/legacy/a.md", "x/legacy/a.md"),
            ("a}b/legacy", "a}b/legacy/a.md", "axb/legacy/a.md"),
        ] {
            let scratch = ScratchDir {
                dir: dir.to_string(),
                files: 1,
                bytes: 1,
            };
            let glob = scratch.glob();
            let rules = compile_rules([glob.as_str()]).unwrap_or_else(|e| panic!("{glob}: {e}"));
            assert!(rules.is_match(inside), "{glob} must exclude {inside}");
            assert!(!rules.is_match(outside), "{glob} must not exclude {outside}");
        }
    }

    #[test]
    fn detect_counts_by_extension_and_applies_the_default_scope() {
        let tmp = repo();
        let report = detect(tmp.path()).expect("detect");
        assert_eq!(
            report.by_ext["md"],
            ExtCount { total: 14, kept: 13 },
            "CLAUDE.md is found but dropped"
        );
        assert_eq!(
            report.by_ext["pdf"],
            ExtCount { total: 2, kept: 1 },
            "ignored/ is never walked"
        );
        assert_eq!(report.by_ext["html"], ExtCount { total: 1, kept: 0 });
        assert_eq!(
            report.by_ext["csv"],
            ExtCount { total: 1, kept: 0 },
            "data is outside include"
        );
    }

    #[test]
    fn detect_reports_scratch_dirs_with_their_in_scope_documents() {
        let tmp = repo();
        let report = detect(tmp.path()).expect("detect");
        assert_eq!(
            report.scratch,
            [
                ScratchDir {
                    dir: "legacy".into(),
                    files: HIGH_CONFIDENCE_MIN_DOCS,
                    bytes: 2048 * HIGH_CONFIDENCE_MIN_DOCS as u64
                },
                ScratchDir {
                    dir: "sandbox".into(),
                    files: 1,
                    bytes: 10
                },
            ]
        );
    }

    #[test]
    fn yes_excludes_only_high_confidence_directories_and_comments_the_rest() {
        let tmp = repo();
        let report = detect(tmp.path()).expect("detect");
        let (excluded, commented) =
            resolve(&report, Mode::Yes, &mut std::io::empty(), &mut Vec::new()).expect("resolve");
        assert_eq!(excluded, ["legacy/**"]);
        assert_eq!(commented.len(), 1);
        assert!(
            commented[0].starts_with("sandbox/**  # sandbox/ (1 documents"),
            "{commented:?}"
        );
    }

    #[test]
    fn interactive_answers_decide_each_directory() {
        let tmp = repo();
        let report = detect(tmp.path()).expect("detect");
        let mut answers = std::io::Cursor::new(b"e\nk\n".to_vec());
        let mut prompts = Vec::new();
        let (excluded, commented) = resolve(&report, Mode::Interactive, &mut answers, &mut prompts).expect("resolve");
        assert_eq!(excluded, ["legacy/**"]);
        assert!(commented.is_empty(), "keep leaves no suggestion: {commented:?}");
        let prompts = String::from_utf8(prompts).unwrap();
        assert!(prompts.contains("legacy/ (10 documents, 20 KiB)"), "{prompts}");

        let mut skip = std::io::Cursor::new(b"\n\n".to_vec());
        let (excluded, commented) = resolve(&report, Mode::Interactive, &mut skip, &mut Vec::new()).expect("resolve");
        assert!(excluded.is_empty());
        assert_eq!(commented.len(), 2, "an empty answer skips");
    }

    #[test]
    fn rendered_lists_parse_and_hold_every_default_entry() {
        let lists = render_lists(&["legacy/**".to_string()], &["sandbox/**".to_string()]);
        let scaffold = apply_to_scaffold(crate::cli::init::INIT_SCAFFOLD_TOML, &lists);
        assert!(!scaffold.contains(SCAFFOLD_PLACEHOLDER));
        let cfg = crate::config::parse_str(&scaffold).expect("scaffold with the scope parses and validates");
        let mut expected = default_document_exclude();
        expected.push("legacy/**".to_string());
        assert_eq!(cfg.documents.exclude, expected);
        assert_eq!(cfg.documents.include, proposed_include());
        assert!(lists.contains("#   sandbox/**"));
    }

    #[test]
    fn in_scope_count_drops_the_excluded_directories() {
        let tmp = repo();
        let report = detect(tmp.path()).expect("detect");
        let kept = report.totals().kept;
        assert_eq!(kept_after_exclusions(&report, &[]), kept);
        assert_eq!(kept_after_exclusions(&report, &["legacy/**".to_string()]), kept - 10);
        let text = render_counts(&report, &["legacy/**".to_string()]);
        assert!(
            text.contains(&format!("in scope after exclusions: {}", kept - 10)),
            "{text}"
        );
    }

    #[test]
    fn counts_print_before_and_after_per_extension() {
        let tmp = repo();
        let report = detect(tmp.path()).expect("detect");
        let text = render_counts(&report, &["legacy/**".to_string()]);
        assert!(
            text.lines()
                .any(|l| l.trim_start().starts_with(".pdf") && l.ends_with("2 -> 1")),
            "{text}"
        );
        assert!(text.contains("also excluded: legacy/**"), "{text}");
    }
}
