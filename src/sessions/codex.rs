//! Codex transcripts: `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, or `.jsonl.zst`
//! once Codex has compressed a rollout it no longer writes to (`codex-rs/rollout` `compression.rs`);
//! [`super::contain::reader`] decompresses those.
//!
//! The record shapes follow the `openai/codex` sources (`codex-rs/history` `RolloutLine` /
//! `RolloutItem`, `codex-rs/protocol` `SessionMeta`, `ResponseItem`, `TokenCountEvent`). Each line is
//! an envelope `{timestamp, ordinal?, type, payload}`. The first is `session_meta`, whose payload
//! carries `id`, `cwd` and `cli_version`, and for a subagent a `source` of `{"subagent": ...}`,
//! `session_id` (the root thread), `parent_thread_id` and `subagent_history_start_ordinal`; a
//! rollout without it (the format before the envelope) names no working directory and is reported
//! as unsupported and unattributed. Records of a subagent's rollout before its start ordinal are the
//! parent's history it inherited, and are skipped; a rollout in Legacy history mode writes no
//! `ordinal`, so there the prefix is the lines before the start ordinal by position. A fork, a
//! revert and a subagent with forked context copy earlier records with their `call_id`, and their
//! token counts with the same cumulative totals, which [`super::Seen`] counts once. Then:
//! `response_item` payloads `function_call` (`name`, `namespace` (`mcp__<server>` for an MCP tool;
//! absent or `functions` for a built-in), `arguments` as a JSON string, `call_id`),
//! `custom_tool_call` (`name`, `namespace`, `input`, `call_id`; the code-mode `exec` script is read
//! by [`super::code_mode`]), `local_shell_call` (`action.command`), `tool_search_call`, and the
//! `*_output` of a call (`call_id`, `output`, a string or content items whose header carries the
//! exit status); and `event_msg` payloads
//! `token_count` (the rise of `info.total_token_usage.output_tokens`, else
//! `info.last_token_usage.output_tokens`). Messages, reasoning and command output are never kept.

use std::io::BufRead;
use std::path::Path;

use serde::Deserialize;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use super::code_mode::{self, Arg};
use super::contain::{self, Found, Line};
use super::shell;
use super::tally::{Call, Family};
use super::{
    Outcome, Parsed, Scope, Seen, TranscriptParser, basemind_reads_path, basemind_tool, lenient, version_check, widen,
};

/// The `cli_version` majors whose record shape this parser was written against.
pub const SUPPORTED_MAJORS: &[u64] = &[0];

/// `sessions/YYYY/MM/DD/`: three directory levels below the root.
const DEPTH: usize = 3;

/// The code-mode tool: its input is a JavaScript script.
const CODE_MODE_TOOL: &str = "exec";

/// The namespace of the built-in tools, the same as none (`codex-rs/protocol` `tool_name.rs`).
const DEFAULT_NAMESPACE: &str = "functions";

pub struct Codex;

#[derive(Deserialize)]
struct Envelope {
    #[serde(default, deserialize_with = "lenient")]
    timestamp: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    ordinal: Option<u64>,
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    kind: Option<String>,
    payload: Option<Payload>,
}

/// Every field a payload kind this parser reads may carry. Loosely typed fields are `Value` or
/// lenient (absent when of another type), so a record whose other fields changed type still parses.
#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    cli_version: Option<String>,
    id: Option<Value>,
    session_id: Option<Value>,
    forked_from_id: Option<Value>,
    parent_thread_id: Option<Value>,
    source: Option<Value>,
    thread_source: Option<Value>,
    #[serde(default, deserialize_with = "lenient")]
    subagent_history_start_ordinal: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    namespace: Option<String>,
    arguments: Option<Value>,
    input: Option<Value>,
    #[serde(default, deserialize_with = "lenient")]
    call_id: Option<String>,
    /// Read for its failure flag only; the output text is never held as a value.
    output: Option<Failed>,
    action: Option<Value>,
    info: Option<Value>,
}

impl TranscriptParser for Codex {
    fn host(&self) -> super::Host {
        super::Host::Codex
    }

    fn transcripts(&self, root: &Path, _scope: &Scope, found: &mut Found) {
        contain::walk(root, root, DEPTH, found);
    }

    fn parse(&self, reader: &mut dyn BufRead, scope: &Scope, seen: &mut Seen) -> Outcome {
        let mut line = Vec::new();
        let mut first: Option<Envelope> = None;
        while first.is_none() {
            match contain::next_line(reader, &mut line) {
                Line::End => return Outcome::Empty,
                Line::Blank => continue,
                Line::TooLong => return unattributed("not JSON lines"),
                Line::Read => {}
            }
            match serde_json::from_slice::<Envelope>(&line) {
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
        let Some(cwd) = meta.cwd.as_deref() else {
            return unattributed("session_meta without cwd");
        };
        let Some(located) = scope.locate(cwd) else {
            return Outcome::OtherDirectory;
        };
        let checkout_path = located.checkout.path.clone();
        let Some(version) = meta.cli_version.as_deref() else {
            return attributed("no cli_version".to_owned());
        };
        let version = match version_check(version, SUPPORTED_MAJORS, "cli_version") {
            Ok(printable) => printable,
            Err(reason) => return attributed(reason),
        };
        let own_start = meta.subagent_history_start_ordinal;
        let mut session = Parsed {
            checkout: located.checkout.label.clone(),
            thread: text(meta.id.as_ref()),
            parent: parent(&meta),
            format_version: version,
            started: None,
            ended: None,
            tally: scope.tally_for(&located),
            skipped_lines: 0,
        };
        if let Some(timestamp) = &first.timestamp {
            widen(&mut session.started, &mut session.ended, timestamp);
        }
        let mut state = State {
            turn: Vec::new(),
            seen,
            copied: false,
            output_total: 0,
            lineage: lineage(&meta),
        };
        // The position of a line after `session_meta` (ordinal 0): a Legacy-mode rollout writes no
        // ordinals, and its inherited prefix is then the lines before the start ordinal.
        let mut position: u64 = 0;
        loop {
            match contain::next_line(reader, &mut line) {
                Line::End => break,
                Line::Blank => continue,
                Line::TooLong => {
                    position += 1;
                    session.skipped_lines += 1;
                    continue;
                }
                Line::Read => position += 1,
            }
            let Ok(envelope) = serde_json::from_slice::<Envelope>(&line) else {
                session.skipped_lines += 1;
                continue;
            };
            if own_start.is_some_and(|start| envelope.ordinal.unwrap_or(position) < start) {
                // The parent's history: not this subagent's, but its token totals are the baseline.
                if let Some(payload) = &envelope.payload
                    && envelope.kind.as_deref() == Some("event_msg")
                    && payload.kind.as_deref() == Some("token_count")
                {
                    state.output_delta(payload.info.as_ref());
                }
                continue;
            }
            if let Some(timestamp) = &envelope.timestamp {
                widen(&mut session.started, &mut session.ended, timestamp);
            }
            if let Some(payload) = envelope.payload {
                // A turn records the directory it ran in, which shell paths are relative to.
                if envelope.kind.as_deref() == Some("turn_context")
                    && let Some(moved) = payload.cwd.as_deref().and_then(|cwd| scope.locate(cwd))
                    && moved.checkout.path == checkout_path
                {
                    session.tally.set_offset(moved.offset);
                }
                record(&mut session, envelope.kind.as_deref(), payload, &mut state);
            }
        }
        Outcome::Session(Box::new(session))
    }
}

/// What reading one rollout carries from record to record.
struct State<'a> {
    /// The families of the calls since the last token count.
    turn: Vec<Family>,
    seen: &'a mut Seen,
    /// The last call was copied from an earlier rollout (a fork or a revert), so a token count
    /// without cumulative totals that follows it is copied too.
    copied: bool,
    /// The last cumulative output-token total.
    output_total: u64,
    /// The root thread of this rollout's lineage, which forks, reverts and subagents share
    /// ([`lineage`]): equal totals are a copy only within it.
    lineage: String,
}

impl State<'_> {
    /// The output tokens a `token_count` adds: the rise of the cumulative
    /// `total_token_usage.output_tokens` (the whole total when it fell: a new count), so a
    /// re-emitted count adds nothing; `last_token_usage.output_tokens` when there is no total.
    fn output_delta(&mut self, info: Option<&Value>) -> Option<u64> {
        let info = info?;
        let total = info.pointer("/total_token_usage/output_tokens").and_then(Value::as_u64);
        let last = info.pointer("/last_token_usage/output_tokens").and_then(Value::as_u64);
        match total {
            Some(total) => {
                let delta = total.checked_sub(self.output_total).unwrap_or(total);
                self.output_total = total;
                Some(delta)
            }
            None => {
                let last = last?;
                self.output_total = self.output_total.saturating_add(last);
                Some(last)
            }
        }
    }
}

/// A token count's `total_token_usage` as written, within `lineage`: the key
/// [`Seen::first_token_count`] tells a copied count by. `None` without a cumulative output total,
/// which [`State::output_delta`] then does not use either.
fn totals_key(info: Option<&Value>, lineage: &str) -> Option<String> {
    let totals = info?.get("total_token_usage")?;
    totals
        .get("output_tokens")
        .and_then(Value::as_u64)
        .map(|_| format!("{lineage}\0{totals}"))
}

/// The root thread a rollout descends from: its `session_id`, else the thread it was forked from,
/// else its own `id`. Two unrelated conversations differ in it even when their totals agree.
fn lineage(meta: &Payload) -> String {
    [&meta.session_id, &meta.forked_from_id, &meta.id]
        .into_iter()
        .find_map(|id| text(id.as_ref()))
        .unwrap_or_default()
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// The thread that spawned a subagent rollout. A rollout is a subagent's when its `source` is
/// `{"subagent": ...}`, its `thread_source` is `subagent`, or it names a `parent_thread_id`; a fork
/// or a reverted thread is none of these and stays a session. Its parent is the direct one
/// (`parent_thread_id` or `source.subagent.thread_spawn.parent_thread_id`), which may be a fork or
/// another subagent, else the root thread (`session_id`, when it is not the rollout's own `id`).
fn parent(meta: &Payload) -> Option<String> {
    let subagent = meta.source.as_ref().is_some_and(|s| s.get("subagent").is_some())
        || meta.thread_source.as_ref().and_then(Value::as_str) == Some("subagent")
        || meta.parent_thread_id.is_some();
    if !subagent {
        return None;
    }
    let own = text(meta.id.as_ref());
    text(meta.parent_thread_id.as_ref())
        .or_else(|| {
            text(
                meta.source
                    .as_ref()
                    .and_then(|s| s.pointer("/subagent/thread_spawn/parent_thread_id")),
            )
        })
        .or_else(|| text(meta.session_id.as_ref()).filter(|root| Some(root) != own.as_ref()))
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

fn record(session: &mut Parsed, kind: Option<&str>, payload: Payload, state: &mut State<'_>) {
    let name = payload.name.as_deref().unwrap_or_default();
    let namespace = own_namespace(payload.namespace.as_deref());
    let is_call = matches!(
        (kind, payload.kind.as_deref()),
        (
            Some("response_item"),
            Some("function_call" | "custom_tool_call" | "tool_search_call" | "local_shell_call")
        )
    );
    if is_call {
        state.copied = payload.call_id.as_deref().is_some_and(|id| !state.seen.first_call(id));
        if state.copied {
            return;
        }
    }
    let calls = match (kind, payload.kind.as_deref()) {
        (Some("response_item"), Some("function_call")) => {
            let arguments = payload.arguments.as_ref().and_then(Value::as_str).unwrap_or_default();
            vec![classify_function(namespace, name, arguments)]
        }
        (Some("response_item"), Some("custom_tool_call")) => {
            let input = payload.input.as_ref().and_then(Value::as_str).unwrap_or_default();
            if mcp_basemind(namespace, name) {
                vec![Call::of(Family::Basemind)]
            } else if namespace.is_some() {
                vec![Call::of(Family::Other)]
            } else if name == CODE_MODE_TOOL {
                let nested = code_mode::calls(input, |name, args| classify_tool(name, args));
                if nested.is_empty() {
                    vec![Call::of(Family::Other)]
                } else {
                    // The script's result is one output for all of them, so no call carries the id.
                    for call in nested {
                        state.turn.push(call.family);
                        session.tally.call(None, call);
                    }
                    return;
                }
            } else {
                vec![classify_tool(name, &|key: &str| {
                    key.is_empty().then(|| Arg::Text(input.to_owned()))
                })]
            }
        }
        (Some("response_item"), Some("tool_search_call")) => vec![Call::of(Family::Other)],
        (Some("response_item"), Some("local_shell_call")) => {
            let argv = payload
                .action
                .as_ref()
                .and_then(|a| a.get("command"))
                .map(argv)
                .unwrap_or_default();
            vec![shell::classify(&script(&argv))]
        }
        (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
            if let Some(id) = &payload.call_id
                && state.seen.first_result(id)
            {
                session.tally.result(id, payload.output.is_some_and(|f| f.0));
            }
            return;
        }
        (Some("event_msg"), Some("token_count")) => {
            // A fork copies the counts before its fork point verbatim, so a count with cumulative
            // totals is copied when they were seen before; a reply after the fork point that called
            // no tool carries totals of its own.
            let copied = totals_key(payload.info.as_ref(), &state.lineage)
                .map_or(state.copied, |key| !state.seen.first_token_count(&key));
            if let Some(output) = state.output_delta(payload.info.as_ref()) {
                if !copied {
                    session.tally.tokens(output, &state.turn);
                }
                state.turn.clear();
            }
            return;
        }
        _ => return,
    };
    for call in calls {
        state.turn.push(call.family);
        session.tally.call(payload.call_id.as_deref(), call);
    }
}

/// An MCP call of a basemind server: `namespace` `mcp__<server>` with the tool `name`, or a name
/// that carries the server itself (`mcp__<server>__<tool>`, or `<server>__<tool>` before namespaces).
fn mcp_basemind(namespace: Option<&str>, name: &str) -> bool {
    basemind_tool(name) || namespace.is_some_and(|ns| basemind_tool(&format!("{ns}__{name}")))
}

/// A tool's namespace, or `None` for the default one: absent, empty or `functions`, which holds
/// the built-in tools.
fn own_namespace(namespace: Option<&str>) -> Option<&str> {
    namespace.filter(|ns| !ns.is_empty() && *ns != DEFAULT_NAMESPACE)
}

/// Classify a `function_call` by its namespace, tool name and JSON `arguments`.
pub fn classify_function(namespace: Option<&str>, name: &str, arguments: &str) -> Call {
    let namespace = own_namespace(namespace);
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let lookup = |key: &str| match args.get(if key.is_empty() { "input" } else { key }) {
        Some(Value::String(text)) => Some(Arg::Text(text.clone())),
        Some(list @ Value::Array(_)) => Some(Arg::List(argv(list))),
        _ => None,
    };
    if mcp_basemind(namespace, name) {
        return basemind_call(name, &lookup);
    }
    if namespace.is_some() {
        return Call::of(Family::Other);
    }
    classify_tool(name, &lookup)
}

/// Classify a built-in Codex tool, or a basemind MCP tool named in full, by its arguments; `args("")`
/// is the freeform input.
fn classify_tool(name: &str, args: &dyn Fn(&str) -> Option<Arg>) -> Call {
    if basemind_tool(name) {
        return basemind_call(name, args);
    }
    let command = |key: &str| match args(key)? {
        Arg::Text(script) => Some(script),
        Arg::List(argv) => Some(script(&argv)),
    };
    let script = match name {
        "shell" | "container.exec" | "local_shell" | "shell_command" => command("command"),
        "exec_command" => command("cmd"),
        "apply_patch" => {
            let body = command("input").or_else(|| command("")).unwrap_or_default();
            return shell::patch(&body);
        }
        _ => return Call::of(Family::Other),
    };
    script.map_or_else(|| Call::of(Family::Other), |s| shell::classify(&s))
}

fn basemind_call(name: &str, args: &dyn Fn(&str) -> Option<Arg>) -> Call {
    let text = |key: &str| match args(key) {
        Some(Arg::Text(text)) => Some(text),
        _ => None,
    };
    let path = text("path").filter(|_| basemind_reads_path(name, text("mode").as_deref()));
    Call {
        read: path.into_iter().collect(),
        repo_relative: true,
        ..Call::of(Family::Basemind)
    }
}

fn argv(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|words| words.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
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

/// Whether a tool output reports failure. The output is a string, or content items whose first
/// text is the header. A failure is a header line `Exit code: N` (shell), `Process exited with code
/// N` (`exec_command`) with N non-zero, or `Script failed` (code mode); in older rollouts, a JSON
/// object (or a string holding one) with `success: false` or a non-zero `metadata.exit_code`. Only
/// the header is inspected, and only it is held: every other item is skipped unparsed.
struct Failed(bool);

/// The object an older rollout's output was.
#[derive(Deserialize)]
struct ObjectOutput {
    #[serde(default, deserialize_with = "lenient")]
    success: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    metadata: Option<Value>,
}

impl ObjectOutput {
    fn failed(&self) -> bool {
        self.success == Some(false)
            || self
                .metadata
                .as_ref()
                .and_then(|m| m.get("exit_code"))
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
    }
}

/// The first content item, for its `text`.
#[derive(Deserialize)]
struct Item {
    #[serde(default, deserialize_with = "lenient")]
    text: Option<String>,
}

impl<'de> Deserialize<'de> for Failed {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FailedVisitor;

        impl<'de> Visitor<'de> for FailedVisitor {
            type Value = Failed;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a tool output")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Failed, E> {
                let failed = match serde_json::from_str::<ObjectOutput>(text) {
                    Ok(object) if text.trim_start().starts_with('{') => object.failed(),
                    _ => header_failed(text),
                };
                Ok(Failed(failed))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Failed, A::Error> {
                let object = ObjectOutput::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(Failed(object.failed()))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Failed, A::Error> {
                // An item of another shape is not a header.
                let first = seq.next_element::<Value>()?;
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                let text = first
                    .and_then(|item| Item::deserialize(item).ok())
                    .and_then(|item| item.text);
                Ok(Failed(text.as_deref().is_some_and(header_failed)))
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Failed, E> {
                Ok(Failed(false))
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Failed, E> {
                Ok(Failed(false))
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Failed, E> {
                Ok(Failed(false))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Failed, E> {
                Ok(Failed(false))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Failed, E> {
                Ok(Failed(false))
            }
        }

        deserializer.deserialize_any(FailedVisitor)
    }
}

/// The other lines a tool output header may hold before `Output:`. A line that is none of these
/// ends the header, so output text is never taken for a status.
const HEADER_PREFIXES: &[&str] = &[
    "Chunk ID:",
    "Wall time",
    "Process running with session ID",
    "Original token count:",
    "Total output lines:",
    "Script ",
];

fn header_failed(text: &str) -> bool {
    for line in text.lines() {
        let line = line.trim();
        if line == "Script failed" {
            return true;
        }
        if let Some(code) = line
            .strip_prefix("Exit code: ")
            .or_else(|| line.strip_prefix("Process exited with code "))
        {
            return code.trim().parse::<i64>().is_ok_and(|code| code != 0);
        }
        if !HEADER_PREFIXES.iter().any(|prefix| line.starts_with(prefix)) {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_calls_map_to_families() {
        let shell = classify_function(None, "shell", r#"{"command":["bash","-lc","rg -n foo src"]}"#);
        assert_eq!(shell.family, Family::ShellSearch);
        let exec = classify_function(None, "exec_command", r#"{"cmd":"sed -n '1,20p' src/a.rs"}"#);
        assert_eq!(exec.family, Family::ReadRange);
        assert_eq!(exec.read, vec!["src/a.rs"]);
        let legacy = classify_function(None, "basemind__code_outline", r#"{"path":"src/a.rs"}"#);
        assert_eq!(legacy.family, Family::Basemind);
        let namespaced = classify_function(Some("mcp__basemind"), "code_outline", r#"{"path":"src/a.rs"}"#);
        assert_eq!(
            (namespaced.family, namespaced.read),
            (Family::Basemind, vec!["src/a.rs".to_owned()])
        );
        let grep = classify_function(Some("mcp__basemind"), "code_grep", r#"{"pattern":"x","path":"src/"}"#);
        assert!(grep.read.is_empty(), "code_grep's path is a filter");
        let plugin = classify_function(Some("mcp__plugin_basemind_basemind"), "code", "{}");
        assert_eq!(plugin.family, Family::Basemind);
        // A namespaced tool that shares a built-in's name is not that built-in.
        assert_eq!(
            classify_function(Some("collaboration"), "exec_command", r#"{"cmd":"rg x"}"#).family,
            Family::Other
        );
        assert_eq!(classify_function(None, "update_plan", "{}").family, Family::Other);
        assert_eq!(
            classify_function(Some("functions"), "exec_command", r#"{"cmd":"rg x"}"#).family,
            Family::ShellSearch,
            "the default namespace holds the built-ins"
        );
        let hashed = classify_function(Some("mcp__0123456789ab__basemind"), "code_read", "{}");
        assert_eq!(hashed.family, Family::Basemind);
    }

    fn meta(json: serde_json::Value) -> Payload {
        serde_json::from_value(json).expect("a session_meta payload")
    }

    #[test]
    fn only_a_subagent_names_a_parent() {
        let spawned = meta(serde_json::json!({
            "session_id": "root", "id": "child", "parent_thread_id": "mid",
            "source": {"subagent": {"thread_spawn": {"parent_thread_id": "mid", "depth": 2}}}
        }));
        assert_eq!(parent(&spawned).as_deref(), Some("mid"), "the direct parent");
        let review = meta(serde_json::json!({"id": "child", "source": {"subagent": "review"}}));
        assert_eq!(parent(&review), None, "no thread named");
        let older = meta(serde_json::json!({
            "id": "child",
            "source": {"subagent": {"thread_spawn": {"parent_thread_id": "mid", "depth": 1}}}
        }));
        assert_eq!(parent(&older).as_deref(), Some("mid"));
        let rooted = meta(serde_json::json!({
            "session_id": "root", "id": "child", "source": {"subagent": "review"}
        }));
        assert_eq!(parent(&rooted).as_deref(), Some("root"), "no direct parent named");
        let fork = meta(serde_json::json!({"session_id": "root", "id": "fork", "source": "cli"}));
        assert_eq!(parent(&fork), None, "a fork is a session of its own");
        let root = meta(serde_json::json!({"session_id": "root", "id": "root", "source": "cli"}));
        assert_eq!(parent(&root), None);
    }

    #[test]
    fn a_totals_key_needs_an_output_total_and_is_scoped_to_the_lineage() {
        let info = serde_json::json!({"total_token_usage": {"output_tokens": 10}});
        assert_eq!(
            totals_key(Some(&info), "root").as_deref(),
            Some("root\0{\"output_tokens\":10}")
        );
        assert_ne!(totals_key(Some(&info), "root"), totals_key(Some(&info), "other"));
        for unusable in [
            serde_json::json!({"total_token_usage": {}}),
            serde_json::json!({"total_token_usage": {"output_tokens": "10"}}),
            serde_json::json!({"last_token_usage": {"output_tokens": 10}}),
        ] {
            assert_eq!(totals_key(Some(&unusable), "root"), None, "{unusable}");
        }
    }

    #[test]
    fn a_rollout_s_lineage_is_its_root_thread() {
        let cases = [
            (
                serde_json::json!({"session_id": "root", "id": "fork", "forked_from_id": "mid"}),
                "root",
            ),
            (serde_json::json!({"id": "fork", "forked_from_id": "mid"}), "mid"),
            (serde_json::json!({"id": "solo"}), "solo"),
            (serde_json::json!({}), ""),
        ];
        for (payload, root) in cases {
            assert_eq!(lineage(&meta(payload.clone())), root, "{payload}");
        }
    }

    fn failed(output: Value) -> bool {
        serde_json::from_value::<Failed>(output).expect("an output").0
    }

    #[test]
    fn failures_are_read_from_the_output_shape() {
        assert!(failed(Value::String(
            r#"{"output":"x","metadata":{"exit_code":1}}"#.into()
        )));
        assert!(!failed(Value::String(
            r#"{"output":"x","metadata":{"exit_code":0}}"#.into()
        )));
        assert!(failed(Value::String("Exit code: 2\nWall time: 1s".into())));
        assert!(failed(Value::String(
            "Chunk ID: 1\nWall time: 0.1 seconds\nProcess exited with code 1\nOutput:\nx".into()
        )));
        assert!(!failed(Value::String(
            "Wall time: 0.1 seconds\nProcess exited with code 0\nOutput:\nExit code: 1".into()
        )));
        assert!(failed(serde_json::json!([
            {"type": "input_text", "text": "Script failed\nWall time 0.1 seconds\nOutput:\n"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
        ])));
        assert!(!failed(serde_json::json!([
            {"type": "input_text", "text": "Script completed\nWall time 0.1 seconds\nOutput:\n"}
        ])));
        assert!(!failed(serde_json::json!(["Exit code: 1"])), "an item of another shape");
        assert!(failed(serde_json::json!({"content": "x", "success": false})));
        assert!(!failed(Value::String("plain output\nExit code: 1".into())));
        assert!(!failed(serde_json::json!(7)));
        assert!(!failed(Value::String("[1, 2]".into())));
    }
}
