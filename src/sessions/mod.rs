//! `basemind sessions`: how agents worked in this repository, measured from the hosts' own local
//! session transcripts (issue #128, ADR-0049).
//!
//! Each host has a [`TranscriptParser`]: [`claude::Claude`] reads `~/.claude/projects/<slug>/*.jsonl`
//! and the subagent transcripts filed under each session, and [`codex::Codex`] reads
//! `~/.codex/sessions/**/*.jsonl`. A
//! transcript counts only when its recorded working directory is inside this repository's main
//! checkout or one of its worktrees. A subagent is reported on its own and attributed to its parent
//! session. The roots are fixed and derived from the home directory alone ([`SessionRoots`]);
//! [`contain`] holds the walk, which follows no symlink and only reads.
//!
//! Nothing here echoes transcript text: a parser keeps tool names, file paths named as arguments,
//! error flags, timestamps and token counts, and the report carries counts and repository-relative
//! paths. A path outside every checkout is counted, never shown. This module runs in the CLI
//! process only; the daemon and the MCP surface never reach it.

pub mod claude;
pub mod codex;
pub mod contain;
pub mod shell;
pub mod tally;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Serialize;

use contain::{Found, RootState};
use tally::{Counts, RepeatedRead, Tally};

/// The host a transcript came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
}

/// The checkouts of this repository: the main one and every linked worktree.
#[derive(Debug, Clone)]
pub struct Scope {
    checkouts: Vec<Checkout>,
}

impl Scope {
    /// An explicit list of `(path, label)` checkouts.
    pub fn new(checkouts: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        let checkouts = checkouts
            .into_iter()
            .map(|(path, label)| {
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                let alias = (canonical != path).then_some(path);
                Checkout {
                    path: canonical,
                    alias,
                    label,
                }
            })
            .collect();
        Self { checkouts }
    }

    /// The repository at `root`, its main checkout and its linked worktrees; `root` alone when it is
    /// not in a git repository.
    pub fn discover(root: &Path) -> Self {
        let mut list: Vec<(PathBuf, String)> = Vec::new();
        if let Ok(repo) = gix::discover(root) {
            if let Some(main) = repo.main_repo().ok().and_then(|m| m.workdir().map(Path::to_path_buf)) {
                list.push((main, "main".to_owned()));
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
        if list.is_empty() {
            list.push((root.to_path_buf(), "main".to_owned()));
        }
        Self::new(list)
    }

    pub fn checkouts(&self) -> &[Checkout] {
        &self.checkouts
    }

    /// The checkout `cwd` is in: the checkout directory itself or any directory below it, the
    /// deepest checkout when one is nested in another. A `cwd` with a `..` component is matched only
    /// by what it resolves to.
    pub fn matching(&self, cwd: &str) -> Option<&Checkout> {
        let cwd = Path::new(cwd);
        let lexical = !cwd.components().any(|c| c == std::path::Component::ParentDir);
        let canonical = cwd.canonicalize().ok();
        self.checkouts
            .iter()
            .filter(|c| {
                let under = |base: &Path| lexical && cwd.starts_with(base);
                under(&c.path)
                    || c.alias.as_deref().is_some_and(under)
                    || canonical
                        .as_deref()
                        .is_some_and(|resolved| resolved.starts_with(&c.path))
            })
            .max_by_key(|c| c.path.components().count())
    }

    /// A tally whose paths are made relative to `checkout` first, then to every other checkout.
    pub fn tally_for(&self, checkout: &Checkout) -> Tally {
        let mut paths = Vec::new();
        for c in std::iter::once(checkout).chain(self.checkouts.iter()) {
            paths.push(c.path.clone());
            paths.extend(c.alias.clone());
        }
        Tally::new(paths)
    }
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
}

/// One host's transcript format.
pub trait TranscriptParser {
    fn host(&self) -> Host;

    /// The candidate transcripts under the canonical `root` for `scope`.
    fn transcripts(&self, root: &Path, scope: &Scope, found: &mut Found);

    /// Parse one transcript, stopping as soon as it is known to belong elsewhere.
    fn parse(&self, reader: &mut dyn BufRead, scope: &Scope) -> Outcome;
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
}

/// Collect the report for `scope` from `roots`. `since` (`YYYY-MM-DD`) keeps the sessions and
/// subagents active on or after that day; one with no timestamp is kept.
pub fn collect(roots: &SessionRoots, scope: &Scope, since: Option<&str>) -> Report {
    let parsers: [&dyn TranscriptParser; 2] = [&claude::Claude, &codex::Codex];
    let mut report = Report::default();
    // The thread id of each reported session, by host, for attributing subagents.
    let mut threads: Vec<(Host, String)> = Vec::new();
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
        let mut transcripts: Vec<(PathBuf, Option<String>)> = found
            .files
            .into_iter()
            .map(|file| (file, None))
            .chain(found.subagents.into_iter().map(|(file, parent)| (file, Some(parent))))
            .collect();
        transcripts.sort();
        for (file, filed_under) in transcripts {
            let Ok(handle) = contain::open(&file) else {
                report.unreadable += 1;
                continue;
            };
            let id = file
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            match parser.parse(&mut BufReader::new(handle), scope) {
                Outcome::Session(parsed) => {
                    let active = parsed.ended.as_deref().or(parsed.started.as_deref());
                    if let (Some(since), Some(active)) = (since, active)
                        && active < since
                    {
                        continue;
                    }
                    let parsed = *parsed;
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
                    match filed_under.or(parsed.parent) {
                        Some(parent) => report.subagents.push(SubagentReport {
                            parent,
                            attributed: false,
                            session,
                        }),
                        None => {
                            threads.push((host, parsed.thread.unwrap_or(id)));
                            report.sessions.push(session);
                        }
                    }
                }
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
    attribute(&mut report, &threads);
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

/// Attribute each subagent to the reported session of its host whose thread id it names, and add
/// its counts to that session's `subagents`. `threads[i]` is the thread id of `report.sessions[i]`.
fn attribute(report: &mut Report, threads: &[(Host, String)]) {
    for subagent in &mut report.subagents {
        let host = subagent.session.host;
        let Some(index) = threads
            .iter()
            .position(|(h, thread)| *h == host && *thread == subagent.parent)
        else {
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

/// A basemind MCP tool name: `mcp__<server>__<tool>` (Claude Code) or `<server>__<tool>` (Codex)
/// whose server name contains `basemind`.
fn basemind_tool(name: &str) -> bool {
    let rest = name.strip_prefix("mcp__").unwrap_or(name);
    rest.split_once("__")
        .is_some_and(|(server, _)| server.contains("basemind"))
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
}
