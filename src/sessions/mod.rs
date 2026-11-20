//! `basemind sessions`: how agents worked in this repository, measured from the hosts' own local
//! session transcripts (issue #128, ADR-0049).
//!
//! Each host has a [`TranscriptParser`]: [`claude::Claude`] reads `~/.claude/projects/<slug>/*.jsonl`
//! and the subagent transcripts filed under each session, and [`codex::Codex`] reads
//! `~/.codex/sessions/**/*.jsonl`, where a subagent is a rollout of its own naming its parent. A
//! transcript counts only when its recorded working directory is inside this repository's main
//! checkout or one of its worktrees, a removed one included ([`worktrees`]). A subagent is reported
//! on its own and attributed to its parent session. The roots are fixed and derived from the home
//! directory alone ([`SessionRoots`]); [`contain`] holds the walk, which follows no symlink and only
//! reads.
//!
//! Nothing here echoes transcript text: a parser keeps tool names, file paths named as arguments,
//! error flags, timestamps and token counts, and the report carries counts and repository-relative
//! paths. A path outside every checkout is counted, never shown. This module runs in the CLI
//! process only; the daemon and the MCP surface never reach it.

pub mod claude;
pub mod code_mode;
pub mod codex;
pub mod contain;
pub mod lexical;
pub mod shell;
pub mod tally;
pub mod worktrees;

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ahash::{AHashMap, AHashSet};
use serde::{Deserialize, Serialize};

use contain::{Found, RootState};
use lexical::Lexical;
use tally::{Counts, RepeatedRead, Tally};
use worktrees::WorktreeDir;

/// The host a transcript came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    Claude,
    Codex,
}

impl Host {
    pub fn as_str(self) -> &'static str {
        match self {
            Host::Claude => "claude",
            Host::Codex => "codex",
        }
    }
}

/// The fixed transcript roots. Nothing in a repository's configuration moves them (ADR-0049 rule 1).
#[derive(Debug, Clone)]
pub struct SessionRoots {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
}

impl SessionRoots {
    /// The roots under `home`: `.claude/projects` and `.codex/sessions`.
    pub fn under_home(home: &Path) -> Self {
        Self {
            claude_projects: home.join(".claude").join("projects"),
            codex_sessions: home.join(".codex").join("sessions"),
        }
    }

    fn root(&self, host: Host) -> &Path {
        match host {
            Host::Claude => &self.claude_projects,
            Host::Codex => &self.codex_sessions,
        }
    }
}

/// One checkout a transcript may have run in.
#[derive(Debug, Clone)]
pub struct Checkout {
    /// Canonical path.
    pub path: PathBuf,
    /// The path as git recorded it, when it differs (a symlinked home).
    pub alias: Option<PathBuf>,
    /// `main` for the main checkout, else the worktree's directory name.
    pub label: String,
    /// `path` and `alias`, as transcript paths are compared with them.
    lexical: Vec<Lexical>,
}

/// The checkouts of this repository: the main one and every linked worktree, and the directories
/// its removed worktrees are recognised in ([`worktrees`]).
#[derive(Debug, Clone)]
pub struct Scope {
    checkouts: Vec<Checkout>,
    /// Deepest first, so a directory nested in another is tried before it.
    worktree_dirs: Vec<WorktreeDir>,
    /// The canonical common git directory, when the scope was discovered from a repository.
    common: Option<PathBuf>,
    /// Whether each directory already looked at is a removed worktree.
    removed: RefCell<AHashMap<PathBuf, bool>>,
}

impl Scope {
    /// An explicit list of `(path, label)` checkouts.
    pub fn new(checkouts: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        let checkouts = checkouts
            .into_iter()
            .map(|(path, label)| {
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                let alias = (canonical != path).then_some(path);
                let lexical = std::iter::once(&canonical)
                    .chain(alias.as_ref())
                    .map(|p| Lexical::parse(&p.to_string_lossy()))
                    .collect();
                Checkout {
                    path: canonical,
                    alias,
                    label,
                    lexical,
                }
            })
            .collect();
        Self {
            checkouts,
            worktree_dirs: Vec::new(),
            common: None,
            removed: RefCell::new(AHashMap::new()),
        }
    }

    /// The repository at `root`, its main checkout and its linked worktrees; `root` alone when it is
    /// not in a git repository.
    pub fn discover(root: &Path) -> Self {
        let mut list: Vec<(PathBuf, String)> = Vec::new();
        let mut common = None;
        let mut linked_from = 0;
        if let Ok(repo) = gix::discover(root) {
            common = repo.common_dir().canonicalize().ok();
            if let Some(main) = repo.main_repo().ok().and_then(|m| m.workdir().map(Path::to_path_buf)) {
                list.push((main, "main".to_owned()));
                linked_from = 1;
            }
            for worktree in repo.worktrees().unwrap_or_default() {
                if let Ok(base) = worktree.base() {
                    let label = base
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "worktree".to_owned());
                    list.push((base, label));
                }
            }
        }
        let found = !list.is_empty();
        if !found {
            list.push((root.to_path_buf(), "main".to_owned()));
        }
        let mut scope = Self::new(list);
        if let Some(common) = common.filter(|_| found) {
            let linked = scope.checkouts.get(linked_from..).unwrap_or_default();
            scope.worktree_dirs = worktrees::dirs(linked, &scope.checkouts, &common);
            scope.common = Some(common);
        }
        scope
    }

    pub fn checkouts(&self) -> &[Checkout] {
        &self.checkouts
    }

    /// The directories a removed worktree of this repository is recognised in, deepest first.
    pub fn worktree_dirs(&self) -> &[WorktreeDir] {
        &self.worktree_dirs
    }

    /// The checkout `cwd` is in: the checkout directory itself or any directory below it, the
    /// deepest checkout when one is nested in another. A `cwd` with a `..` component is matched only
    /// by what it resolves to. A removed worktree ([`worktrees`]) is a checkout too.
    pub fn matching(&self, cwd: &str) -> Option<Cow<'_, Checkout>> {
        self.locate(cwd).map(|located| located.checkout)
    }

    /// [`Scope::matching`], with where `cwd` is inside that checkout. `cwd` is compared as text in
    /// either platform's shape ([`lexical`]), and by what it resolves to on this filesystem.
    pub fn locate(&self, cwd: &str) -> Option<Located<'_>> {
        let named = Lexical::parse(cwd);
        if !named.is_absolute() {
            return None;
        }
        let climbs = cwd.split(['/', '\\']).any(|part| part == "..");
        let resolved = Path::new(cwd)
            .canonicalize()
            .ok()
            .map(|path| Lexical::parse(&path.to_string_lossy()));
        let below = |forms: &[Lexical]| {
            let lexically = forms.iter().filter(|_| !climbs).find_map(|base| named.strip(base));
            lexically.or_else(|| resolved.as_ref().and_then(|r| r.strip(forms.first()?)))
        };
        let live = self
            .checkouts
            .iter()
            .filter_map(|c| Some((c, below(c.lexical.as_slice())?)))
            .max_by_key(|(c, _)| c.path.components().count());
        let depth = live.as_ref().map_or(0, |(c, _)| c.path.components().count());
        self.removed_worktree(&below, depth).or_else(|| {
            live.map(|(c, offset)| Located {
                checkout: Cow::Borrowed(c),
                offset,
            })
        })
    }

    /// The removed worktree `below` places the working directory in, when it is deeper than `depth`,
    /// the depth of the live checkout it is in: the child of a [`WorktreeDir`] it is at or below.
    fn removed_worktree(&self, below: &dyn Fn(&[Lexical]) -> Option<Vec<String>>, depth: usize) -> Option<Located<'_>> {
        let common = self.common.as_deref()?;
        self.worktree_dirs
            .iter()
            .filter(|dir| dir.path.components().count() >= depth)
            .find_map(|dir| {
                let mut offset = below(dir.lexical.as_slice())?;
                if offset.is_empty() {
                    return None;
                }
                let name = offset.remove(0);
                let path = dir.path.join(&name);
                let removed = *self
                    .removed
                    .borrow_mut()
                    .entry(path.clone())
                    .or_insert_with(|| worktrees::removed(&path, common));
                removed.then(|| Located {
                    checkout: Cow::Owned(Checkout {
                        path,
                        alias: None,
                        label: token(&name, ID_MAX).unwrap_or("unparseable").to_owned(),
                        lexical: dir.lexical.iter().map(|form| form.child(&name)).collect(),
                    }),
                    offset,
                })
            })
    }

    /// A tally whose paths are made relative to `located`'s checkout first, then to every other
    /// checkout; a relative path is taken from `located`'s directory.
    pub fn tally_for(&self, located: &Located<'_>) -> Tally {
        let mut paths = Vec::new();
        for c in std::iter::once(located.checkout.as_ref()).chain(self.checkouts.iter()) {
            paths.extend(c.lexical.iter().cloned());
        }
        Tally::new(paths, located.offset.clone())
    }
}

/// A working directory inside a checkout.
pub struct Located<'a> {
    /// Borrowed from the scope, or owned for a removed worktree.
    pub checkout: Cow<'a, Checkout>,
    /// The directories below the checkout, empty for the checkout itself.
    pub offset: Vec<String>,
}

/// What a parser made of one transcript.
pub enum Outcome {
    Session(Box<Parsed>),
    /// Recorded in a directory outside the scope; not read further.
    OtherDirectory,
    /// No record carried a working directory.
    Empty,
    /// A format the parser does not know. `attributed` is false when the working directory could not
    /// be read either, so the transcript may belong to another repository.
    Unsupported {
        reason: String,
        attributed: bool,
    },
}

/// A transcript of this repository, parsed.
pub struct Parsed {
    pub checkout: String,
    /// The id the host gives this conversation when it differs from the file name, so a subagent
    /// can name it as its parent.
    pub thread: Option<String>,
    /// The session this one is a subagent of, when the transcript records it.
    pub parent: Option<String>,
    pub format_version: Option<String>,
    pub started: Option<String>,
    pub ended: Option<String>,
    pub tally: Tally,
    /// Lines that were not JSON of a known shape, or longer than [`contain::LINE_MAX`].
    pub skipped_lines: u64,
}

/// What one host's transcripts have already counted, by the ids the host gives them. A fork, a
/// resumed or reverted conversation and a subagent spawned with its parent's context copy earlier
/// records with their ids, so a record whose id was seen in an earlier transcript is not counted
/// again; a token count without an id is known by its cumulative totals. Transcripts are read oldest
/// first ([`TranscriptParser::written`]).
#[derive(Debug, Default)]
pub struct Seen {
    calls: AHashSet<String>,
    results: AHashSet<String>,
    responses: AHashSet<String>,
    token_counts: AHashSet<String>,
}

impl Seen {
    /// Whether the tool call `id` is counted for the first time.
    pub fn first_call(&mut self, id: &str) -> bool {
        first(&mut self.calls, id)
    }

    /// Whether the result of the tool call `id` is counted for the first time.
    pub fn first_result(&mut self, id: &str) -> bool {
        first(&mut self.results, id)
    }

    /// Whether the model response `id` is counted for the first time.
    pub fn first_response(&mut self, id: &str) -> bool {
        first(&mut self.responses, id)
    }

    /// Whether the token count keyed `key` is counted for the first time. A host whose token counts
    /// carry no id keys one by its cumulative totals, which a copied count repeats verbatim.
    pub fn first_token_count(&mut self, key: &str) -> bool {
        first(&mut self.token_counts, key)
    }
}

fn first(set: &mut AHashSet<String>, id: &str) -> bool {
    !set.contains(id) && set.insert(id.to_owned())
}

/// One host's transcript format.
pub trait TranscriptParser {
    fn host(&self) -> Host;

    /// The candidate transcripts under the canonical `root` for `scope`.
    fn transcripts(&self, root: &Path, scope: &Scope, found: &mut Found);

    /// When `path` was written, for a host whose file names do not sort by time; transcripts are
    /// read in `(written, path)` order.
    fn written(&self, _path: &Path) -> Option<SystemTime> {
        None
    }

    /// Parse one transcript, stopping as soon as it is known to belong elsewhere. A record whose id
    /// is already in `seen` is not counted.
    fn parse(&self, reader: &mut dyn BufRead, scope: &Scope, seen: &mut Seen) -> Outcome;
}

/// One session in the report.
#[derive(Debug, Clone, Serialize)]
pub struct SessionReport {
    pub host: Host,
    pub id: String,
    pub checkout: String,
    pub format_version: Option<String>,
    pub started: Option<String>,
    pub ended: Option<String>,
    pub adoption: Option<f64>,
    pub counts: Counts,
    pub repeated_reads: Vec<RepeatedRead>,
    /// The subagents attributed to this session; absent on a subagent and on a session without any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagents: Option<SubagentTotals>,
}

/// What the subagents of one session did, summed. The session's own `counts` leave them out.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SubagentTotals {
    pub transcripts: u64,
    pub adoption: Option<f64>,
    pub counts: Counts,
}

/// One subagent transcript in the report.
#[derive(Debug, Clone, Serialize)]
pub struct SubagentReport {
    /// The parent session's `id` when it is in the report, else the id the transcript names.
    pub parent: String,
    /// Whether the parent session is in the report and counts this subagent in its `subagents`.
    pub attributed: bool,
    #[serde(flatten)]
    pub session: SessionReport,
}

/// A transcript of this repository whose format is not supported.
#[derive(Debug, Clone, Serialize)]
pub struct UnsupportedReport {
    pub host: Host,
    pub id: String,
    pub reason: String,
}

/// The state of one host's root.
#[derive(Debug, Clone, Serialize)]
pub struct RootReport {
    pub host: Host,
    /// `present`, `missing`, or `refused: <reason>`.
    pub state: String,
}

/// The sum over every reported session and subagent.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Aggregate {
    pub sessions: u64,
    pub subagents: u64,
    pub adoption: Option<f64>,
    pub counts: Counts,
}

/// Everything `basemind sessions` prints.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub roots: Vec<RootReport>,
    pub sessions: Vec<SessionReport>,
    pub subagents: Vec<SubagentReport>,
    pub aggregate: Aggregate,
    pub unsupported: Vec<UnsupportedReport>,
    /// Unsupported transcripts whose working directory could not be read.
    pub unsupported_unattributed: u64,
    /// Transcripts recorded in other directories, skipped after their working directory.
    pub other_directories: u64,
    pub empty: u64,
    /// Symlinks and escaping files the walk refused.
    pub refused: u64,
    pub unreadable: u64,
    /// Lines of the reported transcripts that were skipped: not JSON of a known shape, or longer
    /// than [`contain::LINE_MAX`].
    pub skipped_lines: u64,
}

/// A transcript of this repository, parsed, before the report.
struct Entry {
    host: Host,
    id: String,
    /// The session the host filed it under (a Claude Code subagent).
    filed_under: Option<String>,
    parsed: Parsed,
}

/// Collect the report for `scope` from `roots`. `since` (`YYYY-MM-DD`) keeps the sessions and
/// subagents active on or after that day; one with no timestamp is kept.
pub fn collect(roots: &SessionRoots, scope: &Scope, since: Option<&str>) -> Report {
    let parsers: [&dyn TranscriptParser; 2] = [&claude::Claude, &codex::Codex];
    let mut report = Report::default();
    let mut entries: Vec<Entry> = Vec::new();
    for parser in parsers {
        let host = parser.host();
        let root = match contain::open_root(roots.root(host)) {
            RootState::Present(root) => root,
            RootState::Missing => {
                report.roots.push(RootReport {
                    host,
                    state: "missing".to_owned(),
                });
                continue;
            }
            RootState::Refused(reason) => {
                report.roots.push(RootReport {
                    host,
                    state: format!("refused: {reason}"),
                });
                continue;
            }
        };
        report.roots.push(RootReport {
            host,
            state: "present".to_owned(),
        });
        let mut found = Found::default();
        parser.transcripts(&root, scope, &mut found);
        report.refused += found.refused;
        let mut transcripts: Vec<(Option<SystemTime>, PathBuf, Option<String>)> = found
            .files
            .into_iter()
            .map(|file| (file, None))
            .chain(found.subagents.into_iter().map(|(file, parent)| (file, Some(parent))))
            .map(|(file, parent)| (parser.written(&file), file, parent))
            .collect();
        transcripts.sort();
        let mut seen = Seen::default();
        for (_, file, filed_under) in transcripts {
            let Ok(mut reader) = contain::reader(&file) else {
                report.unreadable += 1;
                continue;
            };
            let id = contain::transcript_id(&file);
            match parser.parse(&mut reader, scope, &mut seen) {
                Outcome::Session(parsed) => entries.push(Entry {
                    host,
                    id,
                    filed_under,
                    parsed: *parsed,
                }),
                Outcome::OtherDirectory => report.other_directories += 1,
                Outcome::Empty => report.empty += 1,
                Outcome::Unsupported { reason, attributed } => {
                    if attributed {
                        report.unsupported.push(UnsupportedReport { host, id, reason });
                    } else {
                        report.unsupported_unattributed += 1;
                    }
                }
            }
        }
    }
    // The thread id of each reported session and subagent, by host, for attributing subagents.
    let mut threads: Vec<(Host, String)> = Vec::new();
    let mut subagent_threads: Vec<Option<String>> = Vec::new();
    for entry in merge_threads(entries) {
        let Entry {
            host,
            id,
            filed_under,
            parsed,
        } = entry;
        let active = parsed.ended.as_deref().or(parsed.started.as_deref());
        if let (Some(since), Some(active)) = (since, active)
            && active < since
        {
            continue;
        }
        report.skipped_lines += parsed.skipped_lines;
        let (counts, repeated_reads) = parsed.tally.finish();
        let session = SessionReport {
            host,
            id: id.clone(),
            checkout: parsed.checkout,
            format_version: parsed.format_version,
            started: parsed.started,
            ended: parsed.ended,
            adoption: counts.adoption(),
            counts,
            repeated_reads,
            subagents: None,
        };
        // A transcript naming itself as its parent is a session with sidechain records in it.
        match filed_under.or(parsed.parent.filter(|parent| *parent != id)) {
            Some(parent) => {
                subagent_threads.push(parsed.thread);
                report.subagents.push(SubagentReport {
                    parent,
                    attributed: false,
                    session,
                });
            }
            None => {
                threads.push((host, parsed.thread.unwrap_or(id)));
                report.sessions.push(session);
            }
        }
    }
    attribute(&mut report, &threads, &subagent_threads);
    // An attributed subagent shows its session's file name; any other names a transcript string.
    for subagent in report.subagents.iter_mut().filter(|s| !s.attributed) {
        if token(&subagent.parent, ID_MAX).is_none() {
            subagent.parent = "unparseable".to_owned();
        }
    }
    report
        .sessions
        .sort_by(|a, b| a.started.cmp(&b.started).then_with(|| a.id.cmp(&b.id)));
    report.subagents.sort_by(|a, b| {
        a.parent
            .cmp(&b.parent)
            .then_with(|| a.session.started.cmp(&b.session.started))
            .then_with(|| a.session.id.cmp(&b.session.id))
    });
    for session in &report.sessions {
        report.aggregate.counts.add(&session.counts);
    }
    for subagent in &report.subagents {
        report.aggregate.counts.add(&subagent.session.counts);
    }
    report.aggregate.sessions = report.sessions.len() as u64;
    report.aggregate.subagents = report.subagents.len() as u64;
    report.aggregate.adoption = report.aggregate.counts.adoption();
    report
}

/// Fold the transcripts of one thread into the first of them: a reverted Codex thread is a new
/// rollout with the same thread id, and the old one stays.
fn merge_threads(entries: Vec<Entry>) -> Vec<Entry> {
    let mut merged: Vec<Entry> = Vec::with_capacity(entries.len());
    let mut by_thread: AHashMap<(Host, String), usize> = AHashMap::new();
    for entry in entries {
        let key = entry.parsed.thread.clone().map(|thread| (entry.host, thread));
        if let Some(&index) = key.as_ref().and_then(|key| by_thread.get(key)) {
            let into = &mut merged[index].parsed;
            for timestamp in [&entry.parsed.started, &entry.parsed.ended].into_iter().flatten() {
                widen(&mut into.started, &mut into.ended, timestamp);
            }
            into.skipped_lines += entry.parsed.skipped_lines;
            into.tally.absorb(entry.parsed.tally);
            continue;
        }
        if let Some(key) = key {
            by_thread.insert(key, merged.len());
        }
        merged.push(entry);
    }
    merged
}

/// Attribute each subagent to the reported session of its host it descends from: the one whose
/// thread id it names, or, when it names another subagent (a subagent spawned by a subagent), the
/// one that subagent descends from. Its counts are added to that session's `subagents`.
/// `threads[i]` is the thread id of `report.sessions[i]`, `subagent_threads[i]` that of
/// `report.subagents[i]`.
fn attribute(report: &mut Report, threads: &[(Host, String)], subagent_threads: &[Option<String>]) {
    enum Node {
        Session(usize),
        Subagent(usize),
    }
    let mut nodes: AHashMap<(Host, &str), Node> = AHashMap::new();
    for (index, (host, thread)) in threads.iter().enumerate() {
        nodes.entry((*host, thread.as_str())).or_insert(Node::Session(index));
    }
    for (index, (subagent, thread)) in report.subagents.iter().zip(subagent_threads).enumerate() {
        if let Some(thread) = thread {
            nodes
                .entry((subagent.session.host, thread.as_str()))
                .or_insert(Node::Subagent(index));
        }
    }
    let targets: Vec<Option<usize>> = report
        .subagents
        .iter()
        .map(|subagent| {
            let host = subagent.session.host;
            let mut name = subagent.parent.as_str();
            // Each step moves to another subagent, so a cycle ends after one pass over them all.
            for _ in 0..=report.subagents.len() {
                match nodes.get(&(host, name)) {
                    Some(Node::Session(index)) => return Some(*index),
                    Some(Node::Subagent(index)) => name = report.subagents[*index].parent.as_str(),
                    None => return None,
                }
            }
            None
        })
        .collect();
    for (subagent, target) in report.subagents.iter_mut().zip(targets) {
        let Some(index) = target else {
            continue;
        };
        let parent = &mut report.sessions[index];
        let totals = parent.subagents.get_or_insert_with(SubagentTotals::default);
        totals.transcripts += 1;
        totals.counts.add(&subagent.session.counts);
        totals.adoption = totals.counts.adoption();
        subagent.parent = parent.id.clone();
        subagent.attributed = true;
    }
}

/// A basemind MCP tool name: `mcp__<server>__<tool>` (Claude Code), `<server>__<tool>` (older
/// Codex) or `mcp__<environment hash>__<server>__<tool>` (Codex with a remote environment), whose
/// server name contains `basemind`.
fn basemind_tool(name: &str) -> bool {
    name.rsplit_once("__")
        .is_some_and(|(server, _)| server.contains("basemind"))
}

/// A field that is `None` when it is absent or of another type, so one changed field never drops
/// the whole record.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).ok())
}

/// The longest version string printed.
const VERSION_MAX: usize = 32;

/// The longest thread id printed.
const ID_MAX: usize = 128;

/// `text` when it is a plain token of at most `max` bytes of `[0-9A-Za-z._+-]`, the shape of a
/// version or an id. A string taken from a transcript is printed only then, so no transcript text or
/// terminal escape reaches the output.
fn token(text: &str, max: usize) -> Option<&str> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-');
    (!text.is_empty() && text.len() <= max && text.bytes().all(plain)).then_some(text)
}

/// A transcript's version, if it is printable, or why it is not supported: `field` names it.
fn version_check(version: &str, supported: &[u64], field: &str) -> Result<Option<String>, String> {
    let printable = token(version, VERSION_MAX);
    if major(version).is_some_and(|m| supported.contains(&m)) {
        return Ok(printable.map(str::to_owned));
    }
    Err(match printable {
        Some(version) => format!("unknown {field} {version}"),
        None => format!("unparseable {field}"),
    })
}

/// Whether the `path` argument of the basemind tool `name` names a file it reads: `code_read`,
/// `code_outline` and `git_blame`, or the `outline` / `read` and `blame` / `blame_symbol` modes of an
/// older server's `code` and `git` tools. Elsewhere `path` is a filter (`code_grep` takes a
/// substring), so it is no read.
fn basemind_reads_path(name: &str, mode: Option<&str>) -> bool {
    let tool = name.rsplit("__").next().unwrap_or(name);
    match tool {
        "code_read" | "code_outline" | "git_blame" => true,
        "code" => matches!(mode, Some("outline" | "read")),
        "git" => matches!(mode, Some("blame" | "blame_symbol")),
        _ => false,
    }
}

/// The leading integer of a version string (`2.1.3` → 2).
fn major(version: &str) -> Option<u64> {
    version.split('.').next()?.trim().parse().ok()
}

/// Keep the earliest and latest of the ISO-8601 timestamps seen.
fn widen(started: &mut Option<String>, ended: &mut Option<String>, timestamp: &str) {
    if started.as_deref().is_none_or(|s| timestamp < s) {
        *started = Some(timestamp.to_owned());
    }
    if ended.as_deref().is_none_or(|e| timestamp > e) {
        *ended = Some(timestamp.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cwd_matches_the_deepest_checkout_it_is_in() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let main = base.join("repo");
        let nested = main.join(".claude/worktrees/w");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        let scope = Scope::new([(main.clone(), "main".to_owned()), (nested.clone(), "w".to_owned())]);
        let label = |cwd: &Path| scope.matching(&cwd.to_string_lossy()).map(|c| c.label.clone());
        assert_eq!(label(&main).as_deref(), Some("main"));
        assert_eq!(
            label(&main.join("crates/core")).as_deref(),
            Some("main"),
            "need not exist"
        );
        assert_eq!(label(&nested.join("src")).as_deref(), Some("w"));
        assert_eq!(
            label(&base.join("repo-old")),
            None,
            "a path prefix is not a directory prefix"
        );
        assert_eq!(label(&main.join("../other")), None, "`..` is resolved first");
        assert_eq!(label(Path::new("repo/src")), None, "a relative cwd is in no checkout");
    }

    #[test]
    fn a_windows_cwd_matches_its_checkout_on_any_platform() {
        let scope = Scope::new([(PathBuf::from(r"\\?\C:\Users\a\repo"), "main".to_owned())]);
        let offset = |cwd: &str| scope.locate(cwd).map(|l| l.offset);
        assert_eq!(
            offset(r"C:\Users\a\repo\crates\core"),
            Some(vec!["crates".into(), "core".into()])
        );
        assert_eq!(offset("c:/users/a/repo"), Some(Vec::new()));
        assert_eq!(offset(r"C:\Users\a\repo-old"), None);
        assert_eq!(offset(r"repo\src"), None);
    }

    #[test]
    fn basemind_tools_are_named_by_their_server() {
        assert!(basemind_tool("mcp__plugin_basemind_basemind__code"));
        assert!(basemind_tool("basemind__code_outline"));
        assert!(basemind_tool("mcp__0123456789ab__basemind__code_outline"));
        assert!(!basemind_tool("code_outline"));
        assert!(!basemind_tool("mcp__other__basemind_like_tool"));
    }
}
