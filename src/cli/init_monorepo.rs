//! Monorepo onboarding preset for `basemind init`.
//!
//! A large monorepo needs hand-tuned `[scan]` and `[documents]` settings before its first scan is
//! useful. Init walks the tree once, detects Bazel, Terraform, large data directories and tracked
//! directories the exclude floor would drop (a committed `build/`), turns each finding into a
//! `Proposal`, and writes the accepted ones into `basemind.toml` as plain values: `floor_allow`,
//! extra `exclude` globs and `[documents] embed = false`. It prints the expected file count and a
//! rough scan time first, so the numbers are visible before anything is written.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};

use super::init_scope::{COUNTED_EXTENSIONS, DocumentScope, Mode, escape_glob, human_bytes};
use crate::config::rules::{RuleSet, compile_rules};
use crate::scanner_filter;

/// Floor directories init offers to re-include when a tracked one exists. `dist` and `target` are
/// left out: the default `[scan] exclude` lists them too, so `floor_allow` alone would not index them.
const SUGGESTED_FLOOR_DIRS: &[&str] = &["build", "out", "vendor"];

/// The suggested floor directories applied under `--yes`: build output names that in a monorepo
/// usually hold committed scripts and rules. `vendor` is only ever suggested.
const AUTO_FLOOR_DIRS: &[&str] = &["build", "out"];

/// Extensions that mark a file as data rather than source.
const DATA_EXTENSIONS: &[&str] = &[
    "csv", "tsv", "json", "jsonc", "yaml", "yml", "xml", "ndjson", "jsonl", "parquet", "arrow", "avro", "orc", "npy",
    "npz", "h5", "hdf5", "pcap", "pcapng", "sqlite", "db", "dat", "bin",
];

/// Files that make a tree a Bazel workspace when they sit at the root.
const BAZEL_ROOT_MARKERS: &[&str] = &["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"];

/// Build-definition file names that mark Bazel at any depth.
const BAZEL_BUILD_FILES: &[&str] = &["BUILD", "BUILD.bazel"];

/// Globs of generated infrastructure and build state that no query wants: each is proposed only
/// when it matches a file, with the reason shown to the user.
const GENERATED_STATE_GLOBS: &[(&str, &str)] = &[
    ("**/*.tfstate", "Terraform state"),
    ("**/*.tfstate.*", "Terraform state backup"),
    ("**/*.tfplan", "Terraform plan"),
    ("**/.terragrunt-cache/**", "Terragrunt cache"),
    ("**/MODULE.bazel.lock", "Bazel module lockfile"),
];

/// Directories the walk never enters.
const SKIPPED_DIRS: &[&str] = &[".git", ".basemind"];

/// Documents in scope at or above this count make embedding every one of them the dominant cost.
const EMBED_OFF_MIN_DOCS: usize = 2000;

/// Below [`EMBED_OFF_MIN_DOCS`], documents still dominate when they are at least this many and more
/// than half of everything indexed.
const EMBED_OFF_MIN_SHARE_DOCS: usize = 500;

/// Rough indexing throughput, files per second, used only for the printed estimate.
const SCAN_FILES_PER_SECOND: usize = 300;

/// Seconds below which the estimate is printed in seconds, not minutes.
const ESTIMATE_SECONDS_CEILING: usize = 90;

/// Cut-offs for what counts as a data directory or an auto-applied floor directory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Thresholds {
    /// A data directory holds at least this many bytes.
    pub data_min_bytes: u64,
    /// ... and at least this percentage of its files are data files.
    pub data_min_percent: usize,
    /// A floor directory is applied under `--yes` with at least this many files.
    pub floor_auto_min_files: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            data_min_bytes: 50 * 1024 * 1024,
            data_min_percent: 95,
            floor_auto_min_files: 10,
        }
    }
}

/// A directory the exclude floor drops that holds files git does not ignore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FloorDir {
    pub name: String,
    pub files: usize,
    pub bytes: u64,
}

/// A directory made almost entirely of data files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DataDir {
    /// Repo-relative, forward-slash.
    pub dir: String,
    pub files: usize,
    /// Of `files`, those the document tier counts.
    pub docs: usize,
    /// Of `docs`, those the document scope keeps.
    pub kept_docs: usize,
    pub bytes: u64,
}

/// Files matching one of [`GENERATED_STATE_GLOBS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateHit {
    pub glob: &'static str,
    pub what: &'static str,
    /// Matching files outside every data directory and not already claimed by an earlier glob.
    pub files: usize,
    /// Of `files`, those the document tier counts.
    pub docs: usize,
    /// Of `docs`, those the document scope keeps.
    pub kept_docs: usize,
    pub bytes: u64,
}

/// What init found in the repository.
#[derive(Debug, Default)]
pub(crate) struct MonorepoReport {
    pub bazel: bool,
    pub terraform_files: usize,
    /// Files that survive the exclude floor.
    pub scan_files: usize,
    /// Of `scan_files`, those the document tier counts (see [`COUNTED_EXTENSIONS`]).
    pub document_files: usize,
    pub floor_dirs: Vec<FloorDir>,
    pub data_dirs: Vec<DataDir>,
    pub state_hits: Vec<StateHit>,
}

/// A directory tally while walking: files, data files, bytes.
#[derive(Default)]
struct DirTally {
    files: usize,
    data_files: usize,
    doc_files: usize,
    kept_doc_files: usize,
    bytes: u64,
}

/// A file matched by one of [`GENERATED_STATE_GLOBS`].
struct StateFile {
    rel: String,
    bytes: u64,
    is_doc: bool,
    is_kept_doc: bool,
}

/// Walk `root` (honouring `.gitignore`) and report the monorepo findings with the default thresholds.
/// `scope` decides which documents count as kept.
pub(crate) fn detect(root: &Path, scope: &DocumentScope) -> Result<MonorepoReport> {
    detect_with(root, &Thresholds::default(), scope)
}

/// [`detect`] with explicit thresholds.
pub(crate) fn detect_with(root: &Path, thresholds: &Thresholds, scope: &DocumentScope) -> Result<MonorepoReport> {
    let floor_names: Vec<&str> = scanner_filter::floor_dir_names().collect();
    let state_rules = GENERATED_STATE_GLOBS
        .iter()
        .map(|(glob, _)| compile_rules(std::iter::once(*glob)).map_err(anyhow::Error::msg))
        .collect::<Result<Vec<RuleSet>>>()?;

    let mut report = MonorepoReport::default();
    let mut floor: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let mut dirs: BTreeMap<String, DirTally> = BTreeMap::new();
    let mut state: Vec<Vec<StateFile>> = GENERATED_STATE_GLOBS.iter().map(|_| Vec::new()).collect();

    let walker = scanner_filter::ignore_walk_builder(root, true, false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|t| t.is_dir())
                || !SKIPPED_DIRS.iter().any(|d| entry.file_name() == *d)
        })
        .build();
    for entry in walker {
        let entry = entry.context("walk the repository")?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let bytes = entry
            .metadata()
            .with_context(|| format!("read the metadata of {}", entry.path().display()))?
            .len();
        let segments: Vec<&str> = rel.split('/').collect();
        let Some((file_name, dir_segments)) = segments.split_last() else {
            continue;
        };
        if let Some(name) = dir_segments
            .iter()
            .find(|segment| floor_names.iter().any(|pattern| floor_matches(pattern, segment)))
        {
            if SUGGESTED_FLOOR_DIRS.contains(name) {
                let slot = floor.entry((*name).to_string()).or_default();
                slot.0 += 1;
                slot.1 += bytes;
            }
            continue;
        }
        let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        report.scan_files += 1;
        let is_doc = ext.as_deref().is_some_and(|e| COUNTED_EXTENSIONS.contains(&e));
        if is_doc {
            report.document_files += 1;
        }
        let is_kept_doc = is_doc && scope.keeps(&rel);
        if ext.as_deref() == Some("tf") {
            report.terraform_files += 1;
        }
        if (dir_segments.is_empty() && BAZEL_ROOT_MARKERS.contains(file_name)) || BAZEL_BUILD_FILES.contains(file_name)
        {
            report.bazel = true;
        }
        for (slot, rules) in state.iter_mut().zip(&state_rules) {
            if rules.is_match(&rel) {
                slot.push(StateFile {
                    rel: rel.clone(),
                    bytes,
                    is_doc,
                    is_kept_doc,
                });
            }
        }
        let is_data = ext.as_deref().is_some_and(|e| DATA_EXTENSIONS.contains(&e));
        let mut prefix = String::new();
        for segment in dir_segments {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            let tally = dirs.entry(prefix.clone()).or_default();
            tally.files += 1;
            tally.bytes += bytes;
            tally.data_files += usize::from(is_data);
            tally.doc_files += usize::from(is_doc);
            tally.kept_doc_files += usize::from(is_kept_doc);
        }
    }

    report.floor_dirs = floor
        .into_iter()
        .map(|(name, (files, bytes))| FloorDir { name, files, bytes })
        .collect();
    report.data_dirs = outermost_data_dirs(&dirs, thresholds);
    report.state_hits = state_hits(state, &report.data_dirs);
    Ok(report)
}

/// The state hits that remain once files inside a data directory, or matched by an earlier glob,
/// are left to the exclusion that already counts them.
fn state_hits(state: Vec<Vec<StateFile>>, data_dirs: &[DataDir]) -> Vec<StateHit> {
    let mut claimed: HashSet<String> = HashSet::new();
    let mut hits = Vec::new();
    for (&(glob, what), matched) in GENERATED_STATE_GLOBS.iter().zip(state) {
        let mut hit = StateHit {
            glob,
            what,
            files: 0,
            docs: 0,
            kept_docs: 0,
            bytes: 0,
        };
        for file in matched {
            let in_data_dir = data_dirs.iter().any(|d| file.rel.starts_with(&format!("{}/", d.dir)));
            if in_data_dir || !claimed.insert(file.rel) {
                continue;
            }
            hit.files += 1;
            hit.docs += usize::from(file.is_doc);
            hit.kept_docs += usize::from(file.is_kept_doc);
            hit.bytes += file.bytes;
        }
        if hit.files > 0 {
            hits.push(hit);
        }
    }
    hits
}

/// True when the floor pattern name (`build`, `bazel-*`) names the path segment.
fn floor_matches(pattern: &str, segment: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => segment.starts_with(prefix),
        None => pattern == segment,
    }
}

/// Directories that are mostly data and large, keeping only the outermost of a nested pair.
fn outermost_data_dirs(dirs: &BTreeMap<String, DirTally>, thresholds: &Thresholds) -> Vec<DataDir> {
    let mut found: Vec<DataDir> = Vec::new();
    for (dir, tally) in dirs {
        let qualifies = tally.bytes >= thresholds.data_min_bytes
            && tally.data_files * 100 >= tally.files * thresholds.data_min_percent;
        let nested = found.iter().any(|d| dir.starts_with(&format!("{}/", d.dir)));
        if qualifies && !nested {
            found.push(DataDir {
                dir: dir.clone(),
                files: tally.files,
                docs: tally.doc_files,
                kept_docs: tally.kept_doc_files,
                bytes: tally.bytes,
            });
        }
    }
    found
}

/// Which `basemind.toml` value a proposal sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    /// An entry of `[scan] floor_allow`.
    FloorAllow,
    /// A glob appended to `[scan] exclude`.
    ScanExclude,
    /// `[documents] embed = false`.
    EmbedOff,
}

/// One suggested change to the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proposal {
    pub setting: Setting,
    pub entry: String,
    pub describe: String,
    /// Files the change removes from the scan (`ScanExclude`) or adds back to it (`FloorAllow`).
    pub files: usize,
    /// Of `files`, those the document tier counts (`ScanExclude` only).
    pub docs: usize,
    /// Of `docs`, those the document scope keeps (`ScanExclude` only).
    pub kept_docs: usize,
    /// Applied under `--yes`; otherwise only printed as a comment.
    pub confident: bool,
}

/// The proposals for `report`; `kept_docs` is the document count the default scope keeps, before the
/// data and state exclusions proposed here take their own documents out.
pub(crate) fn propose(report: &MonorepoReport, kept_docs: usize, thresholds: &Thresholds) -> Vec<Proposal> {
    let mut out = Vec::new();
    for dir in &report.floor_dirs {
        out.push(Proposal {
            setting: Setting::FloorAllow,
            entry: dir.name.clone(),
            describe: format!(
                "{}/ is in the repository but the exclude floor drops it ({} files, {})",
                dir.name,
                dir.files,
                human_bytes(dir.bytes)
            ),
            files: dir.files,
            docs: 0,
            kept_docs: 0,
            confident: AUTO_FLOOR_DIRS.contains(&dir.name.as_str()) && dir.files >= thresholds.floor_auto_min_files,
        });
    }
    for dir in &report.data_dirs {
        out.push(Proposal {
            setting: Setting::ScanExclude,
            entry: format!("{}/**", escape_glob(&dir.dir)),
            describe: format!(
                "{}/ is a data directory ({} files, {})",
                dir.dir,
                dir.files,
                human_bytes(dir.bytes)
            ),
            files: dir.files,
            docs: dir.docs,
            kept_docs: dir.kept_docs,
            confident: true,
        });
    }
    for hit in &report.state_hits {
        out.push(Proposal {
            setting: Setting::ScanExclude,
            entry: hit.glob.to_string(),
            describe: format!("{} ({} files, {})", hit.what, hit.files, human_bytes(hit.bytes)),
            files: hit.files,
            docs: hit.docs,
            kept_docs: hit.kept_docs,
            confident: true,
        });
    }
    let (excluded_files, excluded_kept_docs) = out
        .iter()
        .filter(|p| p.setting == Setting::ScanExclude && p.confident)
        .fold((0, 0), |(files, kept), p| (files + p.files, kept + p.kept_docs));
    let scope_files = report.scan_files.saturating_sub(excluded_files);
    let kept_docs = kept_docs.saturating_sub(excluded_kept_docs);
    let dominated =
        kept_docs >= EMBED_OFF_MIN_DOCS || (kept_docs >= EMBED_OFF_MIN_SHARE_DOCS && kept_docs * 2 > scope_files);
    if dominated {
        out.push(Proposal {
            setting: Setting::EmbedOff,
            entry: "embed = false".to_string(),
            describe: format!(
                "{kept_docs} documents are in scope, so embedding them would dominate the scan; keyword search still works"
            ),
            files: 0,
            docs: 0,
            kept_docs: 0,
            confident: true,
        });
    }
    out
}

/// The resolved preset: what goes into the config and what stays a comment.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Preset {
    pub applied: Vec<Proposal>,
    /// `<entry>  # <description>` lines for the suggestions that were not applied.
    pub commented: Vec<String>,
}

impl Preset {
    fn entries(&self, setting: Setting) -> Vec<String> {
        self.applied
            .iter()
            .filter(|p| p.setting == setting)
            .map(|p| p.entry.clone())
            .collect()
    }

    fn embed_off(&self) -> bool {
        self.applied.iter().any(|p| p.setting == Setting::EmbedOff)
    }
}

/// Decide each proposal: apply it, or keep it as a commented suggestion.
pub(crate) fn resolve(
    proposals: Vec<Proposal>,
    mode: Mode,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Preset> {
    let mut preset = Preset::default();
    for proposal in proposals {
        let apply = match mode {
            Mode::Yes => proposal.confident,
            Mode::Preview => false,
            Mode::Interactive => ask(&proposal, input, output)?,
        };
        if apply {
            preset.applied.push(proposal);
        } else {
            preset
                .commented
                .push(format!("{}  # {}", proposal.entry, proposal.describe));
        }
    }
    Ok(preset)
}

fn ask(proposal: &Proposal, input: &mut dyn BufRead, output: &mut dyn Write) -> Result<bool> {
    write!(
        output,
        "{}: {}? [a]pply / [s]kip (default skip): ",
        proposal.entry, proposal.describe
    )
    .context("write prompt")?;
    output.flush().context("flush prompt")?;
    let mut line = String::new();
    input.read_line(&mut line).context("read answer")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "a" | "apply" | "y" | "yes"
    ))
}

/// Files the scan is expected to index once `preset` is applied. Documents count as the
/// `kept_docs` the scope keeps (before the applied exclusions), not as every document-like file found.
pub(crate) fn expected_files(report: &MonorepoReport, preset: &Preset, kept_docs: usize) -> usize {
    let mut files = report.scan_files;
    let mut excluded_docs = 0;
    let mut excluded_kept_docs = 0;
    for proposal in &preset.applied {
        match proposal.setting {
            Setting::FloorAllow => files += proposal.files,
            Setting::ScanExclude => {
                files = files.saturating_sub(proposal.files);
                excluded_docs += proposal.docs;
                excluded_kept_docs += proposal.kept_docs;
            }
            Setting::EmbedOff => {}
        }
    }
    let remaining_docs = report.document_files.saturating_sub(excluded_docs);
    files.saturating_sub(remaining_docs) + kept_docs.saturating_sub(excluded_kept_docs)
}

fn estimate(files: usize) -> String {
    let seconds = files / SCAN_FILES_PER_SECOND;
    if seconds < ESTIMATE_SECONDS_CEILING {
        format!("{} s", seconds.max(1))
    } else {
        format!("{} min", seconds.div_ceil(60))
    }
}

/// What init prints before it writes: the layouts detected, the settings applied and the expected
/// file count and scan time.
pub(crate) fn render_summary(report: &MonorepoReport, preset: &Preset, kept_docs: usize) -> String {
    let mut detected = Vec::new();
    if report.bazel {
        detected.push("Bazel".to_string());
    }
    if report.terraform_files > 0 {
        detected.push(format!("Terraform ({} .tf files)", report.terraform_files));
    }
    if !report.data_dirs.is_empty() {
        detected.push(format!("data directories: {}", report.data_dirs.len()));
    }
    if !report.floor_dirs.is_empty() {
        detected.push(format!(
            "tracked directories the floor drops: {}",
            report.floor_dirs.len()
        ));
    }
    let mut out = if detected.is_empty() {
        String::from("monorepo layout: nothing special detected")
    } else {
        format!("monorepo layout: {}", detected.join(", "))
    };
    for proposal in &preset.applied {
        out.push_str(&format!("\n  applied: {} ({})", proposal.entry, proposal.describe));
    }
    let files = expected_files(report, preset, kept_docs);
    out.push_str(&format!(
        "\nexpected index: about {files} files, first scan roughly {} (rough, at {SCAN_FILES_PER_SECOND} files/s)",
        estimate(files)
    ));
    out
}

/// `scaffold` with the preset's settings written in place of the commented defaults.
pub(crate) fn apply_to_scaffold(scaffold: &str, preset: &Preset) -> String {
    let mut out = scaffold.to_string();
    let floor_allow = preset.entries(Setting::FloorAllow);
    if !floor_allow.is_empty() {
        out = out.replacen(
            "# floor_allow = []\n",
            &format!("floor_allow = {}\n", toml_list(&floor_allow)),
            1,
        );
    }
    let exclude = preset.entries(Setting::ScanExclude);
    if !exclude.is_empty() {
        let mut all = crate::config::Config::with_defaults().scan.exclude;
        all.extend(exclude);
        out = out.replacen(
            "# exclude = []\n# Floor entries",
            &format!("exclude = {}\n# Floor entries", toml_list(&all)),
            1,
        );
    }
    if preset.embed_off() {
        out = out.replacen(
            "# embed = true\n# Embedding model preset",
            "embed = false\n# Embedding model preset",
            1,
        );
    }
    if !preset.commented.is_empty() {
        let lines: String = preset
            .commented
            .iter()
            .flat_map(|suggestion| suggestion.lines())
            .map(|line| format!("#   {line}\n"))
            .collect();
        out = out.replacen(
            "[scan]\n",
            &format!("[scan]\n# Suggested by `basemind init`, not applied; add an entry to the key it names to use it:\n{lines}"),
            1,
        );
    }
    out
}

fn toml_list(items: &[String]) -> String {
    let body: String = items.iter().map(|i| format!("    {i:?},\n")).collect();
    format!("[\n{body}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SMALL: Thresholds = Thresholds {
        data_min_bytes: 1000,
        data_min_percent: 95,
        floor_auto_min_files: 3,
    };

    fn scope() -> DocumentScope {
        DocumentScope::new(&[]).expect("scope")
    }

    fn touch(root: &Path, rel: &str, bytes: usize) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn monorepo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        touch(root, "WORKSPACE", 10);
        touch(root, "app/BUILD", 10);
        touch(root, "app/main.py", 10);
        touch(root, "infra/main.tf", 10);
        touch(root, "infra/prod/terraform.tfstate", 10);
        touch(root, "node_modules/pkg/index.js", 10);
        for i in 0..3 {
            touch(root, &format!("build/scripts/s{i}.sh"), 10);
        }
        touch(root, "vendor/lib/a.go", 10);
        for i in 0..4 {
            touch(root, &format!("demo/data/rows{i}.csv"), 400);
        }
        touch(root, "demo/README.md", 10);
        touch(root, "docs/guide.md", 10);
        tmp
    }

    fn apply_all(report: &MonorepoReport, kept_docs: usize) -> Preset {
        resolve(
            propose(report, kept_docs, &SMALL),
            Mode::Yes,
            &mut std::io::empty(),
            &mut Vec::new(),
        )
        .expect("resolve")
    }

    #[test]
    fn detect_finds_bazel_terraform_data_and_floor_directories() {
        let tmp = monorepo();
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        assert!(report.bazel);
        assert_eq!(report.terraform_files, 1);
        assert_eq!(
            report.floor_dirs,
            [
                FloorDir {
                    name: "build".into(),
                    files: 3,
                    bytes: 30
                },
                FloorDir {
                    name: "vendor".into(),
                    files: 1,
                    bytes: 10
                },
            ],
            "node_modules is gitignored and not a suggested directory"
        );
        assert_eq!(
            report.data_dirs,
            [DataDir {
                dir: "demo/data".into(),
                files: 4,
                docs: 4,
                kept_docs: 0,
                bytes: 1600
            }],
            "demo is only 80% data (its README), demo/data qualifies"
        );
        assert_eq!(
            report.state_hits,
            [StateHit {
                glob: "**/*.tfstate",
                what: "Terraform state",
                files: 1,
                docs: 0,
                kept_docs: 0,
                bytes: 10
            }]
        );
        assert_eq!(report.scan_files, 12, "floor and ignored files are not counted");
        assert_eq!(report.document_files, 2 + 4);
    }

    #[test]
    fn nothing_is_proposed_for_an_ordinary_repository() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        touch(tmp.path(), "src/main.rs", 10);
        touch(tmp.path(), "README.md", 10);
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        assert!(!report.bazel);
        assert!(propose(&report, 1, &SMALL).is_empty());
        let preset = apply_all(&report, 1);
        assert_eq!(
            apply_to_scaffold(crate::cli::init::INIT_SCAFFOLD_TOML, &preset),
            crate::cli::init::INIT_SCAFFOLD_TOML
        );
    }

    #[test]
    fn yes_applies_confident_proposals_and_comments_the_rest() {
        let tmp = monorepo();
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        let preset = apply_all(&report, 2);
        let applied: Vec<(Setting, &str)> = preset.applied.iter().map(|p| (p.setting, p.entry.as_str())).collect();
        assert_eq!(
            applied,
            [
                (Setting::FloorAllow, "build"),
                (Setting::ScanExclude, "demo/data/**"),
                (Setting::ScanExclude, "**/*.tfstate"),
            ]
        );
        assert_eq!(preset.commented.len(), 1);
        assert!(
            preset.commented[0].starts_with("vendor  # vendor/ is in the repository"),
            "{:?}",
            preset.commented
        );
    }

    #[test]
    fn preview_applies_nothing_and_interactive_follows_the_answers() {
        let tmp = monorepo();
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        let proposals = propose(&report, 2, &SMALL);
        let total = proposals.len();
        let preview = resolve(proposals.clone(), Mode::Preview, &mut std::io::empty(), &mut Vec::new()).unwrap();
        assert!(preview.applied.is_empty());
        assert_eq!(preview.commented.len(), total);

        let mut answers = std::io::Cursor::new(b"a\n\nx\napply\n".to_vec());
        let mut prompts = Vec::new();
        let preset = resolve(proposals, Mode::Interactive, &mut answers, &mut prompts).unwrap();
        let entries: Vec<&str> = preset.applied.iter().map(|p| p.entry.as_str()).collect();
        assert_eq!(entries, ["build", "**/*.tfstate"]);
        assert!(
            String::from_utf8(prompts)
                .unwrap()
                .contains("vendor: vendor/ is in the repository")
        );
    }

    #[test]
    fn embed_is_turned_off_only_when_documents_dominate() {
        let report = MonorepoReport {
            scan_files: 10_000,
            ..MonorepoReport::default()
        };
        for (kept, off) in [(0, false), (499, false), (2000, true)] {
            assert_eq!(propose(&report, kept, &SMALL).is_empty(), !off, "{kept} documents");
        }
        let docs_heavy = MonorepoReport {
            scan_files: 900,
            ..MonorepoReport::default()
        };
        assert_eq!(propose(&docs_heavy, 500, &SMALL).len(), 1, "more than half of the tree");
        assert!(propose(&docs_heavy, 400, &SMALL).is_empty());
    }

    #[test]
    fn document_share_is_measured_against_the_scope_after_data_exclusions() {
        let report = MonorepoReport {
            scan_files: 3000,
            data_dirs: vec![DataDir {
                dir: "data".into(),
                files: 2400,
                docs: 0,
                kept_docs: 0,
                bytes: 1,
            }],
            ..MonorepoReport::default()
        };
        let proposals = propose(&report, 600, &SMALL);
        assert!(
            proposals.iter().any(|p| p.setting == Setting::EmbedOff),
            "{proposals:?}"
        );
    }

    #[test]
    fn documents_in_excluded_data_directories_do_not_count_toward_embedding() {
        let report = MonorepoReport {
            scan_files: 1000,
            document_files: 600,
            data_dirs: vec![DataDir {
                dir: "data".into(),
                files: 700,
                docs: 550,
                kept_docs: 550,
                bytes: 1,
            }],
            ..MonorepoReport::default()
        };
        assert!(
            propose(&report, 600, &SMALL)
                .iter()
                .all(|p| p.setting != Setting::EmbedOff)
        );
        let preset = apply_all(&report, 600);
        assert_eq!(expected_files(&report, &preset, 600), 300);
    }

    #[test]
    fn excluded_csv_files_do_not_remove_kept_documents_from_the_scope() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        for i in 0..40 {
            touch(tmp.path(), &format!("data/rows{i}.csv"), 400);
        }
        for i in 0..5 {
            touch(tmp.path(), &format!("guides/g{i}.md"), 10);
        }
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        assert_eq!(report.data_dirs.len(), 1);
        assert_eq!(report.data_dirs[0].docs, 40, "csv files are document-like");
        assert_eq!(
            report.data_dirs[0].kept_docs, 0,
            "csv is not in the document include list"
        );
        assert_eq!(report.document_files, 45);
        let kept_docs = 5;
        let preset = apply_all(&report, kept_docs);
        assert_eq!(expected_files(&report, &preset, kept_docs), 5);
    }

    #[test]
    fn documents_in_a_dropped_scratch_directory_are_not_removed_twice() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        for i in 0..150 {
            touch(tmp.path(), &format!("data/rows{i}.csv"), 400);
        }
        for i in 0..3 {
            touch(tmp.path(), &format!("data/legacy/n{i}.md"), 10);
            touch(tmp.path(), &format!("data/live/n{i}.md"), 10);
        }
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        assert_eq!(report.data_dirs[0].kept_docs, 6);
        let scratch = DocumentScope::new(&["data/legacy/**".to_string()]).unwrap();
        let report = detect_with(tmp.path(), &SMALL, &scratch).expect("detect");
        assert_eq!(
            report.data_dirs[0].docs, 156,
            "the broad count still includes every document-like file"
        );
        assert_eq!(
            report.data_dirs[0].kept_docs, 3,
            "legacy documents were already dropped by the scratch glob"
        );
    }

    #[test]
    fn a_state_file_inside_a_data_directory_is_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        for i in 0..40 {
            touch(tmp.path(), &format!("data/rows{i}.csv"), 400);
        }
        touch(tmp.path(), "data/terraform.tfstate", 10);
        touch(tmp.path(), "infra/terraform.tfstate", 10);
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        assert_eq!(report.data_dirs.len(), 1);
        assert_eq!(report.data_dirs[0].files, 41);
        assert_eq!(report.state_hits.len(), 1);
        assert_eq!(report.state_hits[0].files, 1, "only the file outside data/");
    }

    #[test]
    fn a_multi_line_suggestion_is_commented_on_every_line() {
        let preset = Preset {
            commented: vec!["data\nmax_file_bytes = 1\n#  # note".to_string()],
            ..Preset::default()
        };
        let scaffold = apply_to_scaffold(crate::cli::init::INIT_SCAFFOLD_TOML, &preset);
        crate::config::parse_str(&scaffold).expect("every physical line stays a comment");
        assert!(scaffold.contains("#   max_file_bytes = 1\n"), "{scaffold}");
    }

    #[test]
    fn applied_preset_parses_into_the_expected_config() {
        let tmp = monorepo();
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        let preset = apply_all(&report, 2500);
        let scaffold = apply_to_scaffold(crate::cli::init::INIT_SCAFFOLD_TOML, &preset);
        let cfg = crate::config::parse_str(&scaffold).expect("scaffold with the preset parses and validates");
        assert_eq!(cfg.scan.floor_allow, ["build"]);
        let mut expected = crate::config::Config::with_defaults().scan.exclude;
        expected.extend(["demo/data/**".to_string(), "**/*.tfstate".to_string()]);
        assert_eq!(cfg.scan.exclude, expected);
        assert!(!cfg.documents.embed);
        assert!(
            scaffold.contains("#   vendor  # vendor/ is in the repository"),
            "{scaffold}"
        );
    }

    #[test]
    fn expected_files_account_for_each_applied_change() {
        let tmp = monorepo();
        let report = detect_with(tmp.path(), &SMALL, &scope()).expect("detect");
        let preset = apply_all(&report, 6);
        // 12 walked + 3 build files - 4 data files - 1 state file = 10; its 2 documents are all kept
        assert_eq!(expected_files(&report, &preset, 6), 10);
        let summary = render_summary(&report, &preset, 6);
        assert!(
            summary.contains("monorepo layout: Bazel, Terraform (1 .tf files)"),
            "{summary}"
        );
        assert!(summary.contains("about 10 files, first scan roughly 1 s"), "{summary}");
        assert!(summary.contains("applied: build"), "{summary}");
    }

    #[test]
    fn estimate_switches_from_seconds_to_minutes() {
        for (files, text) in [
            (0, "1 s"),
            (300, "1 s"),
            (26_699, "88 s"),
            (27_000, "2 min"),
            (99_000, "6 min"),
        ] {
            assert_eq!(estimate(files), text, "{files} files");
        }
    }
}
