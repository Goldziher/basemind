//! `basemind sessions`: how agents worked in this repository, measured from the hosts' own local
//! session transcripts (issue #128, ADR-0049).
//!
//! Each host has a [`TranscriptParser`]: [`claude::Claude`] reads `~/.claude/projects/<slug>/*.jsonl`
//! and [`codex::Codex`] reads `~/.codex/sessions/**/*.jsonl`. A transcript counts only when its
//! recorded working directory is this repository's main checkout or one of its worktrees. The
//! roots are fixed and derived from the home directory alone ([`SessionRoots`]); [`contain`] holds
//! the walk, which follows no symlink and only reads.
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

    /// The checkout whose directory is exactly `cwd`.
    pub fn matching(&self, cwd: &str) -> Option<&Checkout> {
        let cwd = Path::new(cwd);
        let canonical = cwd.canonicalize().ok();
        self.checkouts.iter().find(|c| {
            c.path == cwd || c.alias.as_deref() == Some(cwd) || canonical.as_deref() == Some(c.path.as_path())
        })
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

/// The sum over every reported session.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Aggregate {
    pub sessions: u64,
    pub adoption: Option<f64>,
    pub counts: Counts,
}

/// Everything `basemind sessions` prints.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub roots: Vec<RootReport>,
    pub sessions: Vec<SessionReport>,
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

/// Collect the report for `scope` from `roots`. `since` (`YYYY-MM-DD`) keeps the sessions active
/// on or after that day; a session with no timestamp is kept.
pub fn collect(roots: &SessionRoots, scope: &Scope, since: Option<&str>) -> Report {
    let parsers: [&dyn TranscriptParser; 2] = [&claude::Claude, &codex::Codex];
    let mut report = Report::default();
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
        found.files.sort();
        for file in &found.files {
            let Ok(handle) = contain::open(file) else {
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
                    let (counts, repeated_reads) = parsed.tally.finish();
                    report.sessions.push(SessionReport {
                        host,
                        id,
                        checkout: parsed.checkout,
                        format_version: parsed.format_version,
                        started: parsed.started,
                        ended: parsed.ended,
                        adoption: counts.adoption(),
                        counts,
                        repeated_reads,
                    });
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
    report
        .sessions
        .sort_by(|a, b| a.started.cmp(&b.started).then_with(|| a.id.cmp(&b.id)));
    for session in &report.sessions {
        report.aggregate.counts.add(&session.counts);
    }
    report.aggregate.sessions = report.sessions.len() as u64;
    report.aggregate.adoption = report.aggregate.counts.adoption();
    report
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
