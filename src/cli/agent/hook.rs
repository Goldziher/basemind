//! `basemind agent hook` — what the plugin hooks ask of the binary (ADR-0021).
//!
//! A hook reads the host's event JSON on stdin. `pre-tool-use` answers a call of one of the three
//! agent tools that carries no `as` with the same input plus a session-scoped `as`, so two
//! sessions in one checkout, and a session and its subagents, read as separate actors. `name`
//! prints that name, for the inbox hook's announcing read. The name is
//! [`hook_sub_name`](crate::comms::v2::identity::hook_sub_name): `<agent_type>-<8 hex>` inside a
//! subagent, `session-<8 hex>` in the main session.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{Map, Value, json};

use crate::comms::v2::identity::hook_sub_name;
use crate::mcp::agent_tools::AGENT_TOOLS;

/// The agent type the main session's name is built from.
const MAIN_SESSION: &str = "session";

/// The hook events `basemind agent hook` answers.
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// `PreToolUse` of an agent tool: add the session-scoped `as` when the call has none.
    PreToolUse,
    /// Print the session-scoped name for the event on stdin.
    Name,
}

/// The session-scoped sub-name for a hook event: the subagent's, when the event came from one.
pub fn event_name(event: &Value) -> Option<String> {
    let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
    let name = match (text("agent_type"), text("agent_id")) {
        ("", "") => hook_sub_name(MAIN_SESSION, text("session_id")),
        (agent_type, agent_id) => hook_sub_name(if agent_type.is_empty() { "agent" } else { agent_type }, agent_id),
    };
    name.map(|name| name.as_str().to_string())
}

/// The hook output for a `PreToolUse` event, or `None` when the call is left as it is: not an
/// agent tool, an `as` already given, or no name to give.
pub fn pre_tool_use(event: &Value) -> Option<Value> {
    let tool = event.get("tool_name").and_then(Value::as_str)?;
    if !AGENT_TOOLS
        .iter()
        .any(|name| tool == *name || tool.ends_with(&format!("__{name}")))
    {
        return None;
    }
    let mut input: Map<String, Value> = match event.get("tool_input") {
        Some(Value::Object(input)) => input.clone(),
        None | Some(Value::Null) => Map::new(),
        Some(_) => return None,
    };
    if input
        .get("as")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())
    {
        return None;
    }
    input.insert("as".to_string(), Value::String(event_name(event)?));
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": input,
        }
    }))
}

/// Answer one hook event read from `input`. Prints nothing for an event it leaves alone; never
/// fails on an event it cannot read, because a hook must not break the call it watches.
pub fn run(event: HookEvent, input: &mut dyn Read, out: &mut dyn Write) -> Result<()> {
    let mut text = String::new();
    input.read_to_string(&mut text).context("read the hook event")?;
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return Ok(());
    };
    match event {
        HookEvent::PreToolUse => {
            if let Some(answer) = pre_tool_use(&parsed) {
                writeln!(out, "{answer}")?;
            }
        }
        HookEvent::Name => {
            if let Some(name) = event_name(&parsed) {
                writeln!(out, "{name}")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod tests;
