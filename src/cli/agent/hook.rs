//! `basemind agent hook` — what the plugin hooks ask of the binary (ADR-0021).
//!
//! A hook reads the host's event JSON on stdin. `pre-tool-use` answers a call of one of the three
//! agent tools that carries no `as` with the same input plus a session-scoped `as`, so two
//! sessions in one checkout, and a session and its subagents, read as separate actors. `name`
//! prints that name, for the inbox hook's announcing read. The name is
//! [`hook_sub_name`](crate::comms::v2::identity::hook_sub_name): `<agent_type>-<8 hex>` inside a
//! subagent, `session-<8 hex>` in the main session.
//!
//! Both answer only on a host whose hook can rewrite an MCP call's input, named by
//! `BASEMIND_HOST` (which the plugin launcher sets). Observed with Claude Code 2.1.325 and Codex
//! 0.169.0: Claude Code applies `updatedInput` as it is; Codex applies it only beside
//! `permissionDecision: "allow"`, which does not approve the call (an MCP tool that needs approval
//! still asks for it). On any other host the tool calls act as the bare principal, so the hooks
//! name nothing and read at the principal's level too: `name` prints nothing.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{Map, Value, json};

use crate::comms::v2::identity::{HOST_ENV, hook_sub_name, host_part};
use crate::mcp::agent_tools::AGENT_TOOLS;

/// The agent type the main session's name is built from.
const MAIN_SESSION: &str = "session";

/// How a host's `PreToolUse` hook must answer for its rewritten input to be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Injection {
    /// `updatedInput` alone (Claude Code).
    Plain,
    /// `updatedInput` beside `permissionDecision: "allow"` (Codex).
    WithAllow,
}

impl Injection {
    /// How the host `BASEMIND_HOST` names takes a rewritten input, or `None` when it cannot.
    pub fn for_host(host: Option<&str>) -> Option<Self> {
        match host_part(host).as_str() {
            "claude" => Some(Self::Plain),
            "codex" => Some(Self::WithAllow),
            _ => None,
        }
    }
}

/// The hook events `basemind agent hook` answers.
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// `PreToolUse` of an agent tool: add the session-scoped `as` when the call has none.
    #[command(after_long_help = crate::cli::help::AGENT_HOOK_PRE_TOOL_USE)]
    PreToolUse,
    /// Print the session-scoped name for the event on stdin.
    #[command(after_long_help = crate::cli::help::AGENT_HOOK_NAME)]
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
/// agent tool, an `as` already given (any value but `null` or `""`, which the tool then judges), or
/// no name to give.
pub fn pre_tool_use(event: &Value, injection: Injection) -> Option<Value> {
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
    match input.get("as") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) if name.is_empty() => {}
        Some(_) => return None,
    }
    input.insert("as".to_string(), Value::String(event_name(event)?));
    let mut answer = json!({
        "hookEventName": "PreToolUse",
        "updatedInput": input,
    });
    if injection == Injection::WithAllow {
        answer["permissionDecision"] = json!("allow");
    }
    Some(json!({ "hookSpecificOutput": answer }))
}

/// Answer one hook event read from `input`, for the host `BASEMIND_HOST` names. Prints nothing for
/// an event it leaves alone or a host that cannot take a rewritten input; never fails on an event
/// it cannot read, because a hook must not break the call it watches.
pub fn run(event: HookEvent, input: &mut dyn Read, out: &mut dyn Write) -> Result<()> {
    let host = std::env::var(HOST_ENV).ok();
    run_for(event, Injection::for_host(host.as_deref()), input, out)
}

/// [`run`] for a host that takes a rewritten input as `injection` says, or none.
pub fn run_for(
    event: HookEvent,
    injection: Option<Injection>,
    input: &mut dyn Read,
    out: &mut dyn Write,
) -> Result<()> {
    let mut text = String::new();
    input.read_to_string(&mut text).context("read the hook event")?;
    let (Some(injection), Ok(parsed)) = (injection, serde_json::from_str::<Value>(&text)) else {
        return Ok(());
    };
    let answer = match event {
        HookEvent::PreToolUse => pre_tool_use(&parsed, injection).map(|answer| answer.to_string()),
        HookEvent::Name => event_name(&parsed),
    };
    if let Some(answer) = answer {
        writeln!(out, "{answer}")?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod tests;
