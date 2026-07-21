//! Model-facing multi-agent room tools, backed by the [`RoomClient`](crate::room::RoomClient) seam.
//!
//! These let the agent itself participate in the room: post a message to peers, read recent room
//! history, and list the peers currently present. Only the outbound `room:post` carries a `comms`
//! permission claim (it reaches other agents, so it defaults to Ask); the two read tools carry a
//! `read` claim and are auto-allowed, so a roster/history check never interrupts the turn.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Tool, ToolCtx, ToolDyn, ToolOutput};
use crate::error::{AgentError, Result};
use crate::permission::PermissionClaim;
use crate::room::RoomClient;

/// The multi-agent room tools available to the agent.
pub fn comms_tools() -> Vec<Arc<dyn ToolDyn>> {
    vec![
        Arc::new(RoomPostTool),
        Arc::new(RoomReadTool),
        Arc::new(RoomListAgentsTool),
    ]
}

/// The connected room from the context, or a clean tool error fed back to the model if none is wired.
fn require_room<'a>(ctx: &'a ToolCtx, tool: &'static str) -> Result<&'a Arc<dyn RoomClient>> {
    ctx.room
        .as_ref()
        .ok_or_else(|| AgentError::Tool(format!("{tool}: no multi-agent room is connected")))
}

/// `room:post` — post a message to the shared room.
struct RoomPostTool;

/// Arguments for [`RoomPostTool`].
#[derive(Deserialize, JsonSchema)]
struct RoomPostArgs {
    /// The message body to post to the room.
    text: String,
    /// An optional subject line; the room derives a short one when absent.
    #[serde(default)]
    subject: Option<String>,
}

#[async_trait]
impl Tool for RoomPostTool {
    type Args = RoomPostArgs;

    fn name(&self) -> &'static str {
        "room:post"
    }

    fn description(&self) -> &'static str {
        "Post a message to the shared multi-agent room so peer agents receive it. Use this to \
         coordinate with other agents working the same repo."
    }

    fn permission(&self, _args: &RoomPostArgs) -> PermissionClaim {
        PermissionClaim::comms("room")
    }

    async fn execute(&self, args: RoomPostArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let room = require_room(ctx, Tool::name(self))?;
        room.post(args.subject, args.text).await?;
        Ok(ToolOutput::ok("posted to the room"))
    }
}

/// `room:read` — read recent room history.
struct RoomReadTool;

/// Arguments for [`RoomReadTool`].
#[derive(Deserialize, JsonSchema)]
struct RoomReadArgs {
    /// Only return messages newer than this microsecond timestamp (all history when absent).
    #[serde(default)]
    since_micros: Option<i64>,
}

#[async_trait]
impl Tool for RoomReadTool {
    type Args = RoomReadArgs;

    fn name(&self) -> &'static str {
        "room:read"
    }

    fn description(&self) -> &'static str {
        "Read recent messages from the shared multi-agent room (oldest first) to see what peers have \
         posted. Returns a JSON array of {from, subject, body}."
    }

    fn permission(&self, _args: &RoomReadArgs) -> PermissionClaim {
        PermissionClaim::read("room:history")
    }

    async fn execute(&self, args: RoomReadArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let room = require_room(ctx, Tool::name(self))?;
        let messages = room.history(args.since_micros).await?;
        let value = serde_json::to_string(&messages)?;
        Ok(ToolOutput::ok(value))
    }
}

/// `room:list_agents` — list the peers currently in the room.
struct RoomListAgentsTool;

/// Arguments for [`RoomListAgentsTool`] (none).
#[derive(Deserialize, JsonSchema)]
struct RoomListAgentsArgs {}

#[async_trait]
impl Tool for RoomListAgentsTool {
    type Args = RoomListAgentsArgs;

    fn name(&self) -> &'static str {
        "room:list_agents"
    }

    fn description(&self) -> &'static str {
        "List the peer agents currently present in the shared room. Returns a JSON array of \
         {id, display}."
    }

    fn permission(&self, _args: &RoomListAgentsArgs) -> PermissionClaim {
        PermissionClaim::read("room:roster")
    }

    async fn execute(&self, _args: RoomListAgentsArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        let room = require_room(ctx, Tool::name(self))?;
        let peers = room.roster().await?;
        let value = serde_json::to_string(&peers)?;
        Ok(ToolOutput::ok(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comms_tools_are_named() {
        let names: Vec<_> = comms_tools().iter().map(|tool| tool.name()).collect();
        assert_eq!(names, vec!["room:post", "room:read", "room:list_agents"]);
    }

    #[test]
    fn room_post_requires_a_comms_claim() {
        let claim = RoomPostTool.permission_of(r#"{"text":"hi peers"}"#).expect("parses");
        assert_eq!(claim, PermissionClaim::comms("room"));
    }

    #[test]
    fn room_reads_are_auto_allowable_read_claims() {
        assert_eq!(
            RoomReadTool.permission_of("{}").expect("parses"),
            PermissionClaim::read("room:history")
        );
        assert_eq!(
            RoomListAgentsTool.permission_of("{}").expect("parses"),
            PermissionClaim::read("room:roster")
        );
    }
}
