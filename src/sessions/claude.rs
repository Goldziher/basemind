//! Claude Code transcripts: `~/.claude/projects/<slug>/<session>.jsonl`, where `<slug>` is the
//! session's working directory with every character outside `[A-Za-z0-9]` replaced by `-`. A session
//! started in a subdirectory of a checkout has its own slug, which extends the checkout's with `-`.
//! A subagent's transcript is filed under its session: `<slug>/<session>/subagents/agent-<id>.jsonl`,
//! or one directory deeper for a workflow run (`subagents/workflows/<run>/agent-<id>.jsonl`). Its
//! records have the same shape, with `isSidechain` set and `sessionId` naming the parent. Older
//! versions wrote it beside the sessions (`<slug>/agent-<id>.jsonl`); a transcript whose first
//! record with a `sessionId` is a sidechain is that session's subagent wherever it is filed.
//!
//! Each line is a JSON record. The ones read here carry `cwd`, `version` and `timestamp`; an
//! `assistant` record's `message` holds `tool_use` blocks (`id`, `name`, `input`) and `usage`
//! (`output_tokens`, repeated on every record of one message `id`), and a `user` record's `message`
//! holds `tool_result` blocks (`tool_use_id`, `is_error`). A resumed or forked session repeats the
//! earlier records with their `id`s, which [`super::Seen`] counts once. Text, thinking and tool
//! output are never deserialized.

use std::io::BufRead;
use std::path::Path;

use ahash::AHashMap;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::contain::{self, Found, Line};
use super::shell;
use super::tally::{Call, Family};
use super::{
    Outcome, Parsed, Scope, Seen, TranscriptParser, basemind_reads_path, basemind_tool, lenient, version_check, widen,
};

/// The `version` majors whose record shape this parser was written against.
pub const SUPPORTED_MAJORS: &[u64] = &[1, 2];

/// `subagents/workflows/<run>/`: two directory levels below a session's `subagents` directory.
const SUBAGENT_DEPTH: usize = 2;

pub struct Claude;

/// The project directory name Claude Code derives from a working directory. A canonical Windows
/// path's verbatim `\\?\` prefix is not part of the directory Claude Code saw.
pub fn slug(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let mut slug = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else {
            // Claude Code replaces UTF-16 code units, so a character outside the BMP is two `-`.
            slug.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    slug
}

/// The longest slug Claude Code keeps whole. Its `sanitizePath` returns a slug of at most this
/// length as it is, and cuts a longer one to this length and appends `-` and a base-36 hash of the
/// whole path, a hash that differs between its runtimes; Claude Code itself then finds the project
/// directory by the cut slug and `-`, and so does [`in_checkout`], its transcripts kept by their
/// `cwd`.
const SLUG_MAX: usize = 200;

/// The scalar fields are lenient: one of another type is absent, and the record still counts.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default, deserialize_with = "lenient")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    is_sidechain: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    version: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default)]
    content: Content,
    #[serde(default, deserialize_with = "lenient")]
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Content {
    Blocks(Vec<Block>),
    Other(IgnoredAny),
    #[default]
    Absent,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    input: Option<Value>,
    #[serde(default, deserialize_with = "lenient")]
    tool_use_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    is_error: Option<bool>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default, deserialize_with = "lenient")]
    output_tokens: Option<u64>,
}

impl TranscriptParser for Claude {
    fn host(&self) -> super::Host {
        super::Host::Claude
    }

    fn transcripts(&self, root: &Path, scope: &Scope, found: &mut Found) {
        let mut slugs: Vec<String> = Vec::new();
        // A removed worktree's sessions are filed under a slug that extends its directory's.
        let checkouts = scope.checkouts().iter().map(|c| (&c.path, &c.alias));
        let dirs = scope.worktree_dirs().iter().map(|d| (&d.path, &d.alias));
        for (path, alias) in checkouts.chain(dirs) {
            for path in std::iter::once(path).chain(alias.iter()) {
                let slug = slug(path);
                if !slugs.contains(&slug) {
                    slugs.push(slug);
                }
            }
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        let mut projects: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| slugs.iter().any(|slug| in_checkout(name, slug)))
            .collect();
        projects.sort();
        for project in &projects {
            if let Some(dir) = contain::child_dir(root, project, found) {
                contain::walk(root, &dir, 0, found);
                subagents(root, &dir, found);
            }
        }
    }

    /// Session files are named by a random id, so a resumed or forked session, which copies the
    /// earlier one's records, is told apart from it by when each was written.
    fn written(&self, path: &Path) -> Option<std::time::SystemTime> {
        std::fs::symlink_metadata(path).and_then(|meta| meta.modified()).ok()
    }

    fn parse(&self, reader: &mut dyn BufRead, scope: &Scope, seen: &mut Seen) -> Outcome {
        let mut parsed: Option<Parsed> = None;
        let mut version_seen = false;
        let mut json_seen = false;
        let mut tokens: AHashMap<String, (u64, Vec<Family>)> = AHashMap::new();
        let mut checkout_path: Option<std::path::PathBuf> = None;
        let mut sidechain_of: Option<Option<String>> = None;
        let mut skipped_lines = 0u64;
        let mut line = Vec::new();
        loop {
            match contain::next_line(reader, &mut line) {
                Line::End => break,
                Line::Blank => continue,
                Line::TooLong => {
                    skipped_lines += 1;
                    continue;
                }
                Line::Read => {}
            }
            let Ok(record) = serde_json::from_slice::<Record>(&line) else {
                skipped_lines += 1;
                continue;
            };
            json_seen = true;
            if sidechain_of.is_none()
                && let Some(session_id) = &record.session_id
            {
                sidechain_of = Some(record.is_sidechain.unwrap_or(false).then(|| session_id.clone()));
            }
            if let Some(cwd) = &record.cwd {
                if let Some(session) = parsed.as_mut() {
                    // Every record carries the directory the session is in now (after a `cd`).
                    if let Some(moved) = scope.locate(cwd)
                        && Some(&moved.checkout.path) == checkout_path.as_ref()
                    {
                        session.tally.set_offset(moved.offset);
                    }
                } else {
                    let Some(located) = scope.locate(cwd) else {
                        return Outcome::OtherDirectory;
                    };
                    checkout_path = Some(located.checkout.path.clone());
                    parsed = Some(Parsed {
                        checkout: located.checkout.label.clone(),
                        thread: None,
                        parent: None,
                        format_version: None,
                        started: None,
                        ended: None,
                        tally: scope.tally_for(&located),
                        skipped_lines: 0,
                    });
                }
            }
            let Some(session) = parsed.as_mut() else {
                continue;
            };
            if !version_seen && let Some(version) = &record.version {
                version_seen = true;
                match version_check(version, SUPPORTED_MAJORS, "version") {
                    Ok(printable) => session.format_version = printable,
                    Err(reason) => {
                        return Outcome::Unsupported {
                            reason,
                            attributed: true,
                        };
                    }
                }
            }
            if let Some(timestamp) = &record.timestamp {
                widen(&mut session.started, &mut session.ended, timestamp);
            }
            if let Some(message) = record.message {
                record_message(session, message, &mut tokens, seen);
            }
        }
        match parsed {
            Some(_) if !version_seen => Outcome::Unsupported {
                reason: "no version field".to_owned(),
                attributed: true,
            },
            Some(mut session) => {
                session.parent = sidechain_of.flatten();
                session.skipped_lines = skipped_lines;
                for (id, (output, families)) in tokens {
                    if seen.first_response(&id) {
                        session.tally.tokens(output, &families);
                    }
                }
                Outcome::Session(Box::new(session))
            }
            None if json_seen => Outcome::Empty,
            None => Outcome::Unsupported {
                reason: "not JSON lines".to_owned(),
                attributed: true,
            },
        }
    }
}

/// Whether project directory `name` may hold sessions of the checkout whose slug is `slug`: the
/// checkout itself or a directory below it. The slug is lossy (`a-b` and `a/b` share one), so a
/// sibling such as `repo-old` passes too; its transcripts are dropped by their recorded `cwd`.
fn in_checkout(name: &str, slug: &str) -> bool {
    match slug.get(..SLUG_MAX).filter(|_| slug.len() > SLUG_MAX) {
        Some(cut) => name.strip_prefix(cut).is_some_and(|rest| rest.starts_with('-')),
        None => name
            .strip_prefix(slug)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('-')),
    }
}

/// The subagent transcripts filed under each session directory of `project`. A session directory or
/// `subagents` directory that is a symlink is refused, as is any symlink below it.
fn subagents(root: &Path, project: &Path, found: &mut Found) {
    let Ok(entries) = std::fs::read_dir(project) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type` does not follow a symlink, and the session walk already refused those.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(session) = entry.file_name().into_string() else {
            continue;
        };
        let Some(dir) = contain::child_dir(&entry.path(), "subagents", found) else {
            continue;
        };
        let mut inner = Found::default();
        contain::walk(root, &dir, SUBAGENT_DEPTH, &mut inner);
        found.refused += inner.refused;
        for file in inner.files {
            if file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("agent-"))
            {
                found.subagents.push((file, session.clone()));
            }
        }
    }
}

fn record_message(
    session: &mut Parsed,
    message: Message,
    tokens: &mut AHashMap<String, (u64, Vec<Family>)>,
    seen: &mut Seen,
) {
    let Content::Blocks(blocks) = message.content else {
        return;
    };
    let mut families = Vec::new();
    for block in blocks {
        match block.kind.as_deref().unwrap_or_default() {
            "tool_use" => {
                let name = block.name.as_deref().unwrap_or_default();
                let call = classify(name, block.input.as_ref().unwrap_or(&Value::Null));
                families.push(call.family);
                if block.id.as_deref().is_none_or(|id| seen.first_call(id)) {
                    session.tally.call(block.id.as_deref(), call);
                }
            }
            "tool_result" => {
                if let Some(id) = &block.tool_use_id
                    && seen.first_result(id)
                {
                    session.tally.result(id, block.is_error.unwrap_or(false));
                }
            }
            _ => {}
        }
    }
    let output = message.usage.and_then(|u| u.output_tokens).unwrap_or(0);
    match message.id {
        Some(id) => {
            let entry = tokens.entry(id).or_insert((0, Vec::new()));
            entry.0 = entry.0.max(output);
            entry.1.extend(families);
        }
        None => session.tally.tokens(output, &families),
    }
}

/// Classify one Claude Code tool call by its name and input.
pub fn classify(name: &str, input: &Value) -> Call {
    let field = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    if basemind_tool(name) {
        let reads = basemind_reads_path(name, field("mode").as_deref());
        return Call {
            read: field("path").filter(|_| reads).into_iter().collect(),
            repo_relative: true,
            ..Call::of(Family::Basemind)
        };
    }
    match name {
        "Grep" | "Glob" => Call::of(Family::SearchTool),
        "Read" => {
            let set = |key: &str| input.get(key).is_some_and(|v| !v.is_null());
            let family = if set("offset") || set("limit") {
                Family::ReadRange
            } else {
                Family::ReadWhole
            };
            Call {
                read: field("file_path").into_iter().collect(),
                ..Call::of(family)
            }
        }
        "Edit" | "MultiEdit" => Call {
            edited: field("file_path").into_iter().collect(),
            ..Call::of(Family::Edit)
        },
        "NotebookEdit" => Call {
            edited: field("notebook_path").into_iter().collect(),
            ..Call::of(Family::Edit)
        },
        "Write" => Call {
            written: field("file_path").into_iter().collect(),
            ..Call::of(Family::Write)
        },
        "Bash" => field("command").map_or_else(|| Call::of(Family::Other), |c| shell::classify(&c)),
        _ => Call::of(Family::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slug_replaces_every_non_alphanumeric_character() {
        assert_eq!(
            slug(Path::new("/Users/a/.worktrees/basemind/feat-x")),
            "-Users-a--worktrees-basemind-feat-x"
        );
        assert_eq!(slug(Path::new(r"\\?\C:\Users\a\repo")), "C--Users-a-repo");
        assert_eq!(slug(Path::new("/w/a\u{1F600}é")), "-w-a---", "UTF-16 code units");
    }

    #[test]
    fn a_project_directory_matches_its_checkout_and_the_directories_below() {
        let checkout = slug(Path::new("/w/repo"));
        assert!(in_checkout("-w-repo", &checkout));
        assert!(in_checkout("-w-repo-crates-core", &checkout));
        assert!(!in_checkout("-w-repository", &checkout));
        assert!(!in_checkout("-w", &checkout));
    }

    /// The `sanitizePath` boundary: a slug of [`SLUG_MAX`] characters is kept whole, one character
    /// more is cut to [`SLUG_MAX`] and given `-` and a hash.
    #[test]
    fn a_long_slug_is_matched_as_claude_code_cuts_it() {
        let checkout = |len: usize| slug(Path::new(&format!("/{}", "d".repeat(len - 1))));
        let whole = checkout(SLUG_MAX);
        assert_eq!(whole.len(), SLUG_MAX);
        assert!(in_checkout(&whole, &whole), "kept whole");
        assert!(
            in_checkout(&format!("{whole}-1a2b3c"), &whole),
            "a subdirectory, whose longer slug is cut to this one"
        );
        assert!(
            !in_checkout(&format!("{whole}d"), &whole),
            "a sibling one character longer"
        );
        let cut = checkout(SLUG_MAX + 1);
        assert!(
            in_checkout(&format!("{}-1a2b3c", &cut[..SLUG_MAX]), &cut),
            "cut and hashed"
        );
        assert!(!in_checkout(&cut, &cut), "a slug past the limit is never kept whole");
        assert!(
            !in_checkout(&format!("{}x-1a2b3c", &cut[..SLUG_MAX]), &cut),
            "the cut is followed by `-`"
        );
    }

    #[test]
    fn tools_map_to_families() {
        let input = serde_json::json!({"file_path": "/r/a.rs", "offset": 10});
        assert_eq!(classify("Read", &input).family, Family::ReadRange);
        assert_eq!(
            classify("Read", &serde_json::json!({"file_path": "/r/a.rs"})).family,
            Family::ReadWhole
        );
        assert_eq!(classify("mcp__basemind__code_read", &input).family, Family::Basemind);
        assert_eq!(
            classify("mcp__plugin_basemind_basemind__code", &input).family,
            Family::Basemind
        );
        assert_eq!(classify("mcp__other__code", &input).family, Family::Other);
        let path = serde_json::json!({"path": "src/a.rs", "pattern": "x"});
        assert_eq!(classify("mcp__basemind__code_read", &path).read, vec!["src/a.rs"]);
        assert!(classify("mcp__basemind__code_grep", &path).read.is_empty(), "a filter");
        let mode = serde_json::json!({"mode": "outline", "path": "src/a.rs"});
        assert_eq!(
            classify("mcp__plugin_basemind_basemind__code", &mode).read,
            vec!["src/a.rs"]
        );
        assert_eq!(classify("Glob", &input).family, Family::SearchTool);
        assert_eq!(
            classify("Bash", &serde_json::json!({"command": "git blame src/a.rs"})).family,
            Family::ShellGit
        );
    }
}
