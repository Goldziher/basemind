//! Claude Code transcripts: `~/.claude/projects/<slug>/<session>.jsonl`, where `<slug>` is the
//! session's working directory with every character outside `[A-Za-z0-9]` replaced by `-`.
//!
//! Each line is a JSON record. The ones read here carry `cwd`, `version` and `timestamp`; an
//! `assistant` record's `message` holds `tool_use` blocks (`id`, `name`, `input`) and `usage`
//! (`output_tokens`, repeated on every record of one message `id`), and a `user` record's `message`
//! holds `tool_result` blocks (`tool_use_id`, `is_error`). Text, thinking and tool output are never
//! deserialized.

use std::io::BufRead;
use std::path::Path;

use ahash::AHashMap;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::contain::{self, Found};
use super::shell;
use super::tally::{Call, Family};
use super::{Outcome, Parsed, Scope, TranscriptParser, basemind_tool, major, widen};

/// The `version` majors whose record shape this parser was written against.
pub const SUPPORTED_MAJORS: &[u64] = &[1, 2];

pub struct Claude;

/// The project directory name Claude Code derives from a working directory.
pub fn slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[derive(Deserialize)]
struct Record {
    cwd: Option<String>,
    version: Option<String>,
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    #[serde(default)]
    content: Content,
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
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    tool_use_id: Option<String>,
    is_error: Option<bool>,
}

#[derive(Deserialize)]
struct Usage {
    output_tokens: Option<u64>,
}

impl TranscriptParser for Claude {
    fn host(&self) -> super::Host {
        super::Host::Claude
    }

    fn transcripts(&self, root: &Path, scope: &Scope, found: &mut Found) {
        let mut slugs: Vec<String> = Vec::new();
        for checkout in scope.checkouts() {
            for path in std::iter::once(&checkout.path).chain(checkout.alias.iter()) {
                let slug = slug(path);
                if !slugs.contains(&slug) {
                    slugs.push(slug);
                }
            }
        }
        for slug in &slugs {
            if let Some(dir) = contain::child_dir(root, slug, found) {
                contain::walk(root, &dir, 0, found);
            }
        }
    }

    fn parse(&self, reader: &mut dyn BufRead, scope: &Scope) -> Outcome {
        let mut parsed: Option<Parsed> = None;
        let mut version_seen = false;
        let mut json_seen = false;
        let mut tokens: AHashMap<String, (u64, Vec<Family>)> = AHashMap::new();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if line.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Record>(&line) else {
                continue;
            };
            json_seen = true;
            if parsed.is_none()
                && let Some(cwd) = &record.cwd
            {
                let Some(checkout) = scope.matching(cwd) else {
                    return Outcome::OtherDirectory;
                };
                parsed = Some(Parsed {
                    checkout: checkout.label.clone(),
                    format_version: None,
                    started: None,
                    ended: None,
                    tally: scope.tally_for(checkout),
                });
            }
            let Some(session) = parsed.as_mut() else {
                continue;
            };
            if !version_seen && let Some(version) = &record.version {
                version_seen = true;
                if !major(version).is_some_and(|m| SUPPORTED_MAJORS.contains(&m)) {
                    return Outcome::Unsupported {
                        reason: format!("unknown version {version}"),
                        attributed: true,
                    };
                }
                session.format_version = Some(version.clone());
            }
            if let Some(timestamp) = &record.timestamp {
                widen(&mut session.started, &mut session.ended, timestamp);
            }
            if let Some(message) = record.message {
                record_message(session, message, &mut tokens);
            }
        }
        match parsed {
            Some(_) if !version_seen => Outcome::Unsupported {
                reason: "no version field".to_owned(),
                attributed: true,
            },
            Some(mut session) => {
                for (_, (output, families)) in tokens {
                    session.tally.tokens(output, &families);
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

fn record_message(session: &mut Parsed, message: Message, tokens: &mut AHashMap<String, (u64, Vec<Family>)>) {
    let Content::Blocks(blocks) = message.content else {
        return;
    };
    let mut families = Vec::new();
    for block in blocks {
        match block.kind.as_str() {
            "tool_use" => {
                let name = block.name.as_deref().unwrap_or_default();
                let call = classify(name, block.input.as_ref().unwrap_or(&Value::Null));
                families.push(call.family);
                session.tally.call(block.id.as_deref(), call);
            }
            "tool_result" => {
                if let Some(id) = &block.tool_use_id {
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
        return Call {
            read: field("path").into_iter().collect(),
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
        assert_eq!(classify("Glob", &input).family, Family::SearchTool);
        assert_eq!(
            classify("Bash", &serde_json::json!({"command": "git blame src/a.rs"})).family,
            Family::ShellGit
        );
    }
}
