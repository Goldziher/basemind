//! The two tools only `basemind serve --lead` lists and answers (ADR-0040): `agent_launch` and
//! `agent_stop`.
//!
//! They are not verb tools. No [`BasemindServer`](super::BasemindServer) router holds them, so the
//! daemon, its HTTP front-end, a plain `serve` and a CLI relay session all answer them as unknown
//! tools; `serve --lead` appends [`tool_definitions`] to its `tools/list` and answers the calls in
//! its own process, never forwarding them. They are not always-loaded and count against no verb-tool
//! byte ceiling of their own: `tests/fixtures/lead_tools_list.json` and its ceiling pin them.
//!
//! A model chooses only what a `LaunchRequest` of `basemind-agents` can carry: the harness among
//! the operator's allowed ones, the brief, the working directory, a model and an effort, and limits
//! that are clamped to the operator's caps. No field names an executable, an environment value, an
//! opt-in, a permission mode or a principal.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::strict::ToolArgs;

/// Launches an agent in another harness.
pub const AGENT_LAUNCH: &str = "agent_launch";
/// Stops a launched agent.
pub const AGENT_STOP: &str = "agent_stop";
/// Every tool only a lead `serve` answers.
pub const LEAD_TOOLS: &[&str] = &[AGENT_LAUNCH, AGENT_STOP];

/// Arguments of `agent_launch`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentLaunchParams {
    /// `claude`, `codex` or `opencode`, one the operator allows.
    pub harness: String,
    /// The agent's first prompt, at most 1 MiB, not starting with `/`.
    pub brief: String,
    /// The agent's own existing worktree, relative to the operator's worktrees root.
    pub working_dir: String,
    /// The model, for Claude Code or Codex.
    #[serde(default)]
    pub model: Option<String>,
    /// The reasoning effort, for Claude Code or Codex.
    #[serde(default)]
    pub effort: Option<String>,
    /// Turns the session may run, lowered to the operator's cap.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Seconds the session may run, lowered to the operator's cap.
    #[serde(default)]
    pub wall_time_s: Option<u64>,
}

/// Arguments of `agent_stop`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentStopParams {
    /// The session id `agent_launch` returned.
    pub session: u64,
}

impl ToolArgs for AgentLaunchParams {
    const TOOL: &'static str = AGENT_LAUNCH;
}

impl ToolArgs for AgentStopParams {
    const TOOL: &'static str = AGENT_STOP;
}

/// `arguments` as `agent_launch`'s, or the message naming what is wrong with them.
pub fn parse_launch(arguments: Value) -> Result<AgentLaunchParams, String> {
    super::strict::parse(arguments)
}

/// `arguments` as `agent_stop`'s, or the message naming what is wrong with them.
pub fn parse_stop(arguments: Value) -> Result<AgentStopParams, String> {
    super::strict::parse(arguments)
}

const LAUNCH_DESCRIPTION: &str = "Use to hand a task to an agent in another harness (Claude Code, \
Codex, OpenCode) that the operator allowed: it starts the agent in `working_dir`, its own existing \
worktree under the operator's worktrees root, with `brief` as its first prompt, and returns the \
session id, the agent's id and a basemind thread both of you are members of; the agent may read and \
post to it unasked. Every other approval the agent asks for is denied; nothing here can allow one. Limits \
are clamped to the operator's caps; a refusal is a stable code. \
Example: {\"harness\":\"claude\",\"brief\":\"Review src/store.rs for races\",\"working_dir\":\"review-store\"}";

const STOP_DESCRIPTION: &str = "Use to stop an agent you launched with agent_launch, by the \
`session` id it returned. Stopping a finished session changes nothing. Example: {\"session\":1}";

/// The `tools/list` entries of the lead tools, in [`LEAD_TOOLS`] order.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        definition::<AgentLaunchParams>(
            "Launch an agent",
            LAUNCH_DESCRIPTION,
            json!({
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }),
        ),
        definition::<AgentStopParams>(
            "Stop a launched agent",
            STOP_DESCRIPTION,
            json!({
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false
            }),
        ),
    ]
}

fn definition<T: ToolArgs>(title: &str, description: &str, annotations: Value) -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null);
    super::tool_contract::compact_schema(&mut schema, true);
    // The params struct's own doc names it for Rust readers; the verb tools' schemas carry none.
    if let Some(object) = schema.as_object_mut() {
        object.remove("description");
    }
    json!({
        "name": T::TOOL,
        "title": title,
        "description": description,
        "inputSchema": schema,
        "annotations": annotations,
    })
}
