//! Per-session counters every transcript parser feeds: the tool family of each call, the read and
//! search history behind "edit without a prior read" and "write before any search", and output
//! tokens by what the turn did. Only counts and repository-relative paths are kept.

use std::path::{Component, Path, PathBuf};

use ahash::{AHashMap, AHashSet};
use serde::Serialize;

/// A file read this many times or more in one session is a repeated read.
pub const REPEATED_READ_THRESHOLD: u32 = 4;

/// What a tool call did, by the question it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// A basemind MCP tool or a `basemind` CLI command.
    Basemind,
    /// The host's own search tools (Claude Code `Grep` / `Glob`).
    SearchTool,
    /// A shell search: `rg`, `grep`, `find`, `fd`, `ag`, `ack`.
    ShellSearch,
    /// A shell history query: `git log`, `git blame`, `git show`, `git grep`.
    ShellGit,
    /// A whole-file read: `Read` without a range, `cat`.
    ReadWhole,
    /// A ranged read: `Read` with `offset` / `limit`, `sed -n`, `head`, `tail`.
    ReadRange,
    /// A change to an existing file: `Edit`, `MultiEdit`, an `apply_patch` update, `sed -i`.
    Edit,
    /// A file written whole: `Write`, an `apply_patch` add, a shell redirect.
    Write,
    /// Anything else.
    Other,
}

impl Family {
    fn is_search(self) -> bool {
        matches!(
            self,
            Family::Basemind | Family::SearchTool | Family::ShellSearch | Family::ShellGit
        )
    }

    fn is_change(self) -> bool {
        matches!(self, Family::Edit | Family::Write)
    }

    fn is_read_or_search(self) -> bool {
        self.is_search() || matches!(self, Family::ReadWhole | Family::ReadRange)
    }
}

/// One classified call: its family, the paths it read and the paths it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub family: Family,
    pub read: Vec<String>,
    /// Existing files changed in place; each needs a prior read.
    pub edited: Vec<String>,
    /// Files written whole; no prior read is expected.
    pub written: Vec<String>,
    /// A relative path names a file from the repository root (a basemind `path` argument), not
    /// from the session's working directory (a shell command, a host tool, a patch).
    pub repo_relative: bool,
}

impl Call {
    pub fn of(family: Family) -> Self {
        Self {
            family,
            read: Vec::new(),
            edited: Vec::new(),
            written: Vec::new(),
            repo_relative: false,
        }
    }
}

/// Output tokens split by what the turn that produced them did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct OutputTokens {
    pub read_search: u64,
    pub edit: u64,
    pub other: u64,
}

/// The counts of one session, or the sum over many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub basemind: u64,
    pub search_tool: u64,
    pub shell_search: u64,
    pub shell_git: u64,
    pub read_whole: u64,
    pub read_range: u64,
    pub edit: u64,
    pub write: u64,
    pub other: u64,
    /// Tool results the host marked as failed.
    pub errors: u64,
    pub basemind_errors: u64,
    pub edits_without_read: u64,
    pub writes_without_search: u64,
    /// Repository files read [`REPEATED_READ_THRESHOLD`] times or more.
    pub files_read_repeatedly: u64,
    /// Distinct paths outside every checkout that were read or changed; never listed.
    pub outside_paths: u64,
    pub output_tokens: OutputTokens,
}

impl Counts {
    /// basemind calls over the calls it can replace: basemind, the host search tools, shell
    /// searches and history queries, and whole-file reads. `None` with no such call.
    pub fn adoption(&self) -> Option<f64> {
        let denominator = self.basemind + self.search_tool + self.shell_search + self.shell_git + self.read_whole;
        (denominator > 0).then(|| self.basemind as f64 / denominator as f64)
    }

    pub fn add(&mut self, other: &Counts) {
        self.basemind += other.basemind;
        self.search_tool += other.search_tool;
        self.shell_search += other.shell_search;
        self.shell_git += other.shell_git;
        self.read_whole += other.read_whole;
        self.read_range += other.read_range;
        self.edit += other.edit;
        self.write += other.write;
        self.other += other.other;
        self.errors += other.errors;
        self.basemind_errors += other.basemind_errors;
        self.edits_without_read += other.edits_without_read;
        self.writes_without_search += other.writes_without_search;
        self.files_read_repeatedly += other.files_read_repeatedly;
        self.outside_paths += other.outside_paths;
        self.output_tokens.read_search += other.output_tokens.read_search;
        self.output_tokens.edit += other.output_tokens.edit;
        self.output_tokens.other += other.output_tokens.other;
    }

    fn count_family(&mut self, family: Family) {
        let slot = match family {
            Family::Basemind => &mut self.basemind,
            Family::SearchTool => &mut self.search_tool,
            Family::ShellSearch => &mut self.shell_search,
            Family::ShellGit => &mut self.shell_git,
            Family::ReadWhole => &mut self.read_whole,
            Family::ReadRange => &mut self.read_range,
            Family::Edit => &mut self.edit,
            Family::Write => &mut self.write,
            Family::Other => &mut self.other,
        };
        *slot += 1;
    }
}

/// A path a transcript named, keyed against the session's checkouts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PathKey {
    /// Repository-relative, forward slashes.
    Repo(String),
    /// Outside every checkout: counted, never printed.
    Outside(String),
}

/// Accumulates one session.
pub struct Tally {
    checkouts: Vec<PathBuf>,
    /// The session's working directory below its checkout.
    offset: PathBuf,
    counts: Counts,
    reads: AHashMap<PathKey, u32>,
    known: AHashSet<PathKey>,
    outside: AHashSet<String>,
    searched: bool,
    calls: AHashMap<String, Family>,
}

impl Tally {
    /// `checkouts` are the directories a path is made relative to: the session's own first.
    /// `offset` is the working directory below it, which a relative path is taken from.
    pub fn new(checkouts: Vec<PathBuf>, offset: PathBuf) -> Self {
        Self {
            checkouts,
            offset,
            counts: Counts::default(),
            reads: AHashMap::new(),
            known: AHashSet::new(),
            outside: AHashSet::new(),
            searched: false,
            calls: AHashMap::new(),
        }
    }

    /// The working directory moved to `offset` below the session's checkout.
    pub fn set_offset(&mut self, offset: PathBuf) {
        self.offset = offset;
    }

    /// Record one call. `id` ties a later [`Tally::result`] to it.
    pub fn call(&mut self, id: Option<&str>, call: Call) {
        self.counts.count_family(call.family);
        if let Some(id) = id {
            self.calls.insert(id.to_owned(), call.family);
        }
        let from_root = call.repo_relative;
        for raw in &call.read {
            let key = self.key(raw, from_root);
            *self.reads.entry(key.clone()).or_insert(0) += 1;
            self.known.insert(key);
        }
        if call.family.is_change() && !self.searched {
            self.counts.writes_without_search += 1;
        }
        for raw in &call.edited {
            let key = self.key(raw, from_root);
            if !self.known.contains(&key) {
                self.counts.edits_without_read += 1;
            }
            self.known.insert(key);
        }
        for raw in &call.written {
            let key = self.key(raw, from_root);
            self.known.insert(key);
        }
        if call.family.is_search() {
            self.searched = true;
        }
    }

    /// Record the outcome of the call `id`; an unknown id still counts its error.
    pub fn result(&mut self, id: &str, is_error: bool) {
        if !is_error {
            return;
        }
        self.counts.errors += 1;
        if self.calls.get(id) == Some(&Family::Basemind) {
            self.counts.basemind_errors += 1;
        }
    }

    /// Add `tokens` of model output from a turn whose calls had `families`: a turn that changed a
    /// file is editing, one that only read or searched is reading, anything else is other.
    pub fn tokens(&mut self, tokens: u64, families: &[Family]) {
        let bucket = if families.iter().any(|f| f.is_change()) {
            &mut self.counts.output_tokens.edit
        } else if !families.is_empty() && families.iter().all(|f| f.is_read_or_search()) {
            &mut self.counts.output_tokens.read_search
        } else {
            &mut self.counts.output_tokens.other
        };
        *bucket += tokens;
    }

    /// The final counts and the repository files read repeatedly, most-read first.
    pub fn finish(mut self) -> (Counts, Vec<RepeatedRead>) {
        let mut repeated: Vec<RepeatedRead> = self
            .reads
            .iter()
            .filter(|(_, n)| **n >= REPEATED_READ_THRESHOLD)
            .filter_map(|(key, n)| match key {
                PathKey::Repo(path) => Some(RepeatedRead {
                    path: path.clone(),
                    reads: *n,
                }),
                PathKey::Outside(_) => None,
            })
            .collect();
        repeated.sort_by(|a, b| b.reads.cmp(&a.reads).then_with(|| a.path.cmp(&b.path)));
        self.counts.files_read_repeatedly = repeated.len() as u64;
        self.counts.outside_paths = self.outside.len() as u64;
        (self.counts, repeated)
    }

    fn key(&mut self, raw: &str, from_root: bool) -> PathKey {
        let offset = if from_root {
            Path::new("")
        } else {
            self.offset.as_path()
        };
        let key = repo_key(&self.checkouts, offset, raw);
        if let PathKey::Outside(path) = &key {
            self.outside.insert(path.clone());
        }
        key
    }
}

/// A repository file read [`REPEATED_READ_THRESHOLD`] times or more in one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepeatedRead {
    pub path: String,
    pub reads: u32,
}

/// Key `raw` against the checkouts: an absolute path under one becomes relative to it, a relative
/// path is taken from `offset` below the session's checkout. `..` is resolved lexically; a path that
/// climbs above its checkout, or is under none, is outside.
fn repo_key(checkouts: &[PathBuf], offset: &Path, raw: &str) -> PathKey {
    let path = Path::new(raw);
    if path.is_absolute() {
        for checkout in checkouts {
            if let Ok(rel) = path.strip_prefix(checkout)
                && let Some(clean) = resolve(Path::new(""), rel)
            {
                return PathKey::Repo(clean);
            }
        }
        return PathKey::Outside(raw.to_owned());
    }
    match resolve(offset, path) {
        Some(clean) => PathKey::Repo(clean),
        None => {
            let base = checkouts.first().map(|c| c.join(offset)).unwrap_or_default();
            PathKey::Outside(base.join(path).to_string_lossy().into_owned())
        }
    }
}

/// `path` taken from the directory `base`, both below one checkout, as a forward-slash path from the
/// checkout (`.` for the checkout itself); `None` when it climbs above the checkout.
fn resolve(base: &Path, path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for component in base.components().chain(path.components()) {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally() -> Tally {
        Tally::new(
            vec![PathBuf::from("/repo"), PathBuf::from("/wt/feature")],
            PathBuf::new(),
        )
    }

    fn read(path: &str) -> Call {
        Call {
            read: vec![path.to_owned()],
            ..Call::of(Family::ReadWhole)
        }
    }

    fn edit(path: &str) -> Call {
        Call {
            edited: vec![path.to_owned()],
            ..Call::of(Family::Edit)
        }
    }

    #[test]
    fn an_edit_of_a_file_read_through_another_checkout_spelling_has_a_prior_read() {
        let mut t = tally();
        t.call(None, read("/repo/src/a.rs"));
        t.call(None, edit("src/a.rs"));
        t.call(None, edit("/wt/feature/src/b.rs"));
        let (counts, _) = t.finish();
        assert_eq!(counts.edits_without_read, 1);
        assert_eq!(counts.writes_without_search, 2);
    }

    #[test]
    fn repeated_reads_list_repository_paths_only() {
        let mut t = tally();
        for _ in 0..4 {
            t.call(None, read("/repo/src/a.rs"));
            t.call(None, read("/etc/hosts"));
        }
        t.call(None, read("../outside.rs"));
        let (counts, repeated) = t.finish();
        assert_eq!(
            repeated,
            vec![RepeatedRead {
                path: "src/a.rs".into(),
                reads: 4
            }]
        );
        assert_eq!(counts.files_read_repeatedly, 1);
        assert_eq!(counts.outside_paths, 2);
    }

    #[test]
    fn a_search_before_a_write_clears_writes_without_search() {
        let mut t = tally();
        t.call(Some("1"), Call::of(Family::Basemind));
        t.result("1", true);
        t.call(
            None,
            Call {
                written: vec!["new.rs".into()],
                ..Call::of(Family::Write)
            },
        );
        let (counts, _) = t.finish();
        assert_eq!(counts.writes_without_search, 0);
        assert_eq!(counts.errors, 1);
        assert_eq!(counts.basemind_errors, 1);
        assert_eq!(counts.adoption(), Some(1.0));
    }

    #[test]
    fn tokens_go_to_the_turn_bucket() {
        let mut t = tally();
        t.tokens(10, &[Family::Basemind, Family::ReadRange]);
        t.tokens(5, &[Family::ReadWhole, Family::Edit]);
        t.tokens(3, &[]);
        t.tokens(2, &[Family::Other, Family::ReadWhole]);
        let (counts, _) = t.finish();
        assert_eq!(
            counts.output_tokens,
            OutputTokens {
                read_search: 10,
                edit: 5,
                other: 5
            }
        );
    }

    #[test]
    fn a_relative_path_is_taken_from_the_working_directory() {
        let mut t = Tally::new(vec![PathBuf::from("/repo")], PathBuf::from("crates/core"));
        for path in [
            "src/lib.rs",
            "./src/lib.rs",
            "../core/src/lib.rs",
            "/repo/crates/core/src/lib.rs",
        ] {
            t.call(None, read(path));
        }
        t.call(
            None,
            Call {
                repo_relative: true,
                ..read("src/main.rs")
            },
        );
        t.call(None, edit("../../src/main.rs"));
        t.call(None, read("../../../etc/hosts"));
        let (counts, repeated) = t.finish();
        assert_eq!(
            repeated,
            vec![RepeatedRead {
                path: "crates/core/src/lib.rs".into(),
                reads: 4
            }]
        );
        assert_eq!((counts.edits_without_read, counts.outside_paths), (0, 1));
    }
}
