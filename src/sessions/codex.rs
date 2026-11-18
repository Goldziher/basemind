//! Codex transcripts: `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
//!
//! Each line is an envelope `{timestamp, type, payload}`. The first is `session_meta`, whose
//! payload carries `cwd` and `cli_version`; a rollout without it (the format before the envelope)
//! names no working directory and is reported as unsupported and unattributed. Then:
//! `response_item` payloads `function_call` (`name`, `arguments` as a JSON string, `call_id`),
//! `custom_tool_call` (`name`, `input`, `call_id`), `local_shell_call` (`action.command`), and their
//! `*_output` (`call_id`, `output`); and `event_msg` payloads `token_count`
//! (`info.last_token_usage.output_tokens`). Messages, reasoning and command output are never kept.

use std::io::BufRead;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::contain::{self, Found};
use super::shell;
use super::tally::{Call, Family};
use super::{Outcome, Parsed, Scope, TranscriptParser, basemind_tool, major, widen};

/// The `cli_version` majors whose record shape this parser was written against.
pub const SUPPORTED_MAJORS: &[u64] = &[0];

/// `sessions/YYYY/MM/DD/`: three directory levels below the root.
const DEPTH: usize = 3;

pub struct Codex;

#[derive(Deserialize)]
struct Envelope {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    cli_version: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    input: Option<String>,
    call_id: Option<String>,
    output: Option<Value>,
    action: Option<Action>,
    info: Option<TokenInfo>,
}

#[derive(Deserialize)]
struct Action {
    command: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TokenInfo {
    last_token_usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    output_tokens: Option<u64>,
}

impl TranscriptParser for Codex {
    fn host(&self) -> super::Host {
        super::Host::Codex
    }

    fn transcripts(&self, root: &Path, _scope: &Scope, found: &mut Found) {
        contain::walk(root, root, DEPTH, found);
    }

    fn parse(&self, reader: &mut dyn BufRead, scope: &Scope) -> Outcome {
        let mut line = String::new();
        let mut first: Option<Envelope> = None;
        while first.is_none() {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return Outcome::Empty,
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => {}
            }
            match serde_json::from_str::<Envelope>(&line) {
                Ok(envelope) => first = Some(envelope),
                Err(_) => return unattributed("not JSON lines"),
            }
        }
        let Some(first) = first else {
            return Outcome::Empty;
        };
        let (Some("session_meta"), Some(meta)) = (first.kind.as_deref(), first.payload) else {
            return unattributed("no session_meta record");
        };
        let Some(cwd) = meta.cwd else {
            return unattributed("session_meta without cwd");
        };
        let Some(checkout) = scope.matching(&cwd) else {
            return Outcome::OtherDirectory;
        };
        let Some(version) = meta.cli_version else {
            return attributed("no cli_version".to_owned());
        };
        if !major(&version).is_some_and(|m| SUPPORTED_MAJORS.contains(&m)) {
            return attributed(format!("unknown cli_version {version}"));
        }
        let mut session = Parsed {
            checkout: checkout.label.clone(),
            format_version: Some(version),
            started: None,
            ended: None,
            tally: scope.tally_for(checkout),
        };
        if let Some(timestamp) = &first.timestamp {
            widen(&mut session.started, &mut session.ended, timestamp);
        }
        let mut turn: Vec<Family> = Vec::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(envelope) = serde_json::from_str::<Envelope>(&line) else {
                continue;
            };
            if let Some(timestamp) = &envelope.timestamp {
                widen(&mut session.started, &mut session.ended, timestamp);
            }
            if let Some(payload) = envelope.payload {
                record(&mut session, envelope.kind.as_deref(), payload, &mut turn);
            }
        }
        Outcome::Session(Box::new(session))
    }
}

fn unattributed(reason: &str) -> Outcome {
    Outcome::Unsupported {
        reason: reason.to_owned(),
        attributed: false,
    }
}

fn attributed(reason: String) -> Outcome {
    Outcome::Unsupported {
        reason,
        attributed: true,
    }
}

fn record(session: &mut Parsed, kind: Option<&str>, payload: Payload, turn: &mut Vec<Family>) {
    let call = match (kind, payload.kind.as_deref()) {
        (Some("response_item"), Some("function_call")) => Some(classify_function(
            payload.name.as_deref().unwrap_or_default(),
            payload.arguments.as_deref().unwrap_or_default(),
        )),
        (Some("response_item"), Some("custom_tool_call")) => Some(match payload.name.as_deref() {
            Some("apply_patch") => shell::patch(payload.input.as_deref().unwrap_or_default()),
            Some(name) if basemind_tool(name) => Call::of(Family::Basemind),
            _ => Call::of(Family::Other),
        }),
        (Some("response_item"), Some("local_shell_call")) => {
            let argv = payload.action.and_then(|a| a.command).unwrap_or_default();
            Some(shell::classify(&script(&argv)))
        }
        (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
            if let Some(id) = &payload.call_id {
                session.tally.result(id, payload.output.as_ref().is_some_and(failed));
            }
            None
        }
        (Some("event_msg"), Some("token_count")) => {
            let output = payload
                .info
                .and_then(|info| info.last_token_usage)
                .and_then(|usage| usage.output_tokens);
            if let Some(output) = output {
                session.tally.tokens(output, turn);
                turn.clear();
            }
            None
        }
        _ => None,
    };
    if let Some(call) = call {
        turn.push(call.family);
        session.tally.call(payload.call_id.as_deref(), call);
    }
}

/// Classify a `function_call` by its tool name and JSON `arguments`.
pub fn classify_function(name: &str, arguments: &str) -> Call {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
    if basemind_tool(name) {
        return Call {
            read: text("path").into_iter().collect(),
            ..Call::of(Family::Basemind)
        };
    }
    let command = |key: &str| match args.get(key) {
        Some(Value::String(script)) => Some(script.clone()),
        Some(Value::Array(argv)) => Some(script(
            &argv
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
        )),
        _ => None,
    };
    let script = match name {
        "shell" | "container.exec" | "local_shell" | "shell_command" => command("command"),
        "exec_command" => command("cmd"),
        "apply_patch" => return shell::patch(&text("input").unwrap_or_default()),
        _ => return Call::of(Family::Other),
    };
    script.map_or_else(|| Call::of(Family::Other), |s| shell::classify(&s))
}

/// The command line an argv runs: the script of `bash -lc <script>`, else the words joined.
fn script(argv: &[String]) -> String {
    if let [shell, flag, script, ..] = argv
        && shell.rsplit('/').next().is_some_and(|s| s.ends_with("sh"))
        && flag.starts_with('-')
        && flag.contains('c')
    {
        return script.clone();
    }
    argv.join(" ")
}

/// A tool output that reports failure: `success: false`, a non-zero `metadata.exit_code`, or an
/// `Exit code: N` header with N non-zero. The output text is inspected for that and dropped.
fn failed(output: &Value) -> bool {
    match output {
        Value::Object(map) => {
            map.get("success").and_then(Value::as_bool) == Some(false)
                || map
                    .get("metadata")
                    .and_then(|m| m.get("exit_code"))
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0)
        }
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(inner @ Value::Object(_)) => failed(&inner),
            _ => text
                .strip_prefix("Exit code: ")
                .and_then(|rest| rest.lines().next())
                .and_then(|code| code.trim().parse::<i64>().ok())
                .is_some_and(|code| code != 0),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_calls_map_to_families() {
        let shell = classify_function("shell", r#"{"command":["bash","-lc","rg -n foo src"]}"#);
        assert_eq!(shell.family, Family::ShellSearch);
        let exec = classify_function("exec_command", r#"{"cmd":"sed -n '1,20p' src/a.rs"}"#);
        assert_eq!(exec.family, Family::ReadRange);
        assert_eq!(exec.read, vec!["src/a.rs"]);
        let mcp = classify_function("basemind__code_outline", r#"{"path":"src/a.rs"}"#);
        assert_eq!(mcp.family, Family::Basemind);
        assert_eq!(classify_function("update_plan", "{}").family, Family::Other);
    }

    #[test]
    fn failures_are_read_from_the_output_shape() {
        assert!(failed(&Value::String(
            r#"{"output":"x","metadata":{"exit_code":1}}"#.into()
        )));
        assert!(!failed(&Value::String(
            r#"{"output":"x","metadata":{"exit_code":0}}"#.into()
        )));
        assert!(failed(&Value::String("Exit code: 2\nWall time: 1s".into())));
        assert!(failed(&serde_json::json!({"content": "x", "success": false})));
        assert!(!failed(&Value::String("plain output".into())));
    }
}
