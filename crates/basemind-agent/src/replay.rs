//! Deterministic scripted-model replay.
//!
//! A [`Scenario`] is a fixed sequence of assistant turns (text and/or tool calls) that drives the
//! real engine with no network and no API key, so a run is repeatable. It backs three callers: the
//! `scripted` example, the TUI `--replay` flag, and the end-to-end tests. Available only under test
//! or the `test-util` feature, since it is built on the scripted [`MockModelClient`].

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use liter_llm::ChatCompletionChunk;
use serde::Deserialize;

use crate::error::Result;
use crate::model::{MockModelClient, ModelClient};
use crate::provider::{ProviderPool, ResolvedRole};
use crate::room::{RoomMessage, RoomPeer, ScriptedIncoming, ScriptedRoom};

/// Routing string reported for the scripted model (status bar, requests).
const SCRIPTED_MODEL: &str = "mock/scripted";

/// A full replay scenario: an optional system prompt, the opening user message, and the scripted
/// assistant turns the model replays in order.
#[derive(Clone, Debug, Deserialize)]
pub struct Scenario {
    /// System prompt for the session; the caller supplies a default when this is `None`.
    #[serde(default)]
    pub system: Option<String>,
    /// The user message that opens the turn.
    pub user: String,
    /// The scripted assistant replies, one per model round.
    pub turns: Vec<ScriptTurn>,
    /// An optional multi-agent room to attach: a static roster plus a timed incoming feed. Drives a
    /// [`ScriptedRoom`] so a PTY/e2e test can exercise the room with no broker.
    #[serde(default)]
    pub room: Option<RoomScript>,
}

/// A scripted multi-agent room: the roster published at start and the peer messages delivered on a
/// timer. Deserialized from the scenario JSON's optional `room` object.
#[derive(Clone, Debug, Deserialize)]
pub struct RoomScript {
    /// The roster of peer agents published once at session start.
    #[serde(default)]
    pub roster: Vec<RoomPeer>,
    /// Peer messages delivered after their `after_ms` delay, in order.
    #[serde(default)]
    pub incoming: Vec<RoomIncoming>,
}

/// One scripted incoming room message: the sender, subject, body, and the delay before delivery.
#[derive(Clone, Debug, Deserialize)]
pub struct RoomIncoming {
    /// The posting peer's id.
    pub from: String,
    /// The message subject (front-matter); empty when omitted.
    #[serde(default)]
    pub subject: String,
    /// The message body.
    pub body: String,
    /// Milliseconds the incoming task waits before delivering this message.
    #[serde(default)]
    pub after_ms: u64,
}

/// One scripted assistant reply: optional text plus zero or more tool calls. An empty `tools` list
/// makes the model stop after this turn; otherwise it requests the tools and continues.
#[derive(Clone, Debug, Deserialize)]
pub struct ScriptTurn {
    /// Assistant text streamed for this round.
    #[serde(default)]
    pub text: Option<String>,
    /// Tool calls the model requests this round.
    #[serde(default)]
    pub tools: Vec<ScriptToolCall>,
}

/// A scripted tool call: the call id, the namespaced tool name, and the JSON arguments object.
#[derive(Clone, Debug, Deserialize)]
pub struct ScriptToolCall {
    /// Provider-style call id (pairs a result with its start).
    pub id: String,
    /// The namespaced tool name (e.g. `code:outline`).
    pub name: String,
    /// The arguments object passed to the tool.
    pub args: serde_json::Value,
}

impl Scenario {
    /// Parse a scenario from a JSON string.
    pub fn from_json(raw: &str) -> Result<Self> {
        Ok(serde_json::from_str(raw)?)
    }

    /// Load a scenario from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        Self::from_json(&std::fs::read_to_string(path)?)
    }

    /// The chunk streams a [`MockModelClient`] replays, one inner `Vec` per round.
    fn chunks(&self) -> Vec<Vec<ChatCompletionChunk>> {
        self.turns
            .iter()
            .map(|turn| {
                let mut chunks = Vec::new();
                if let Some(text) = &turn.text {
                    chunks.push(MockModelClient::text(text));
                }
                for (index, call) in turn.tools.iter().enumerate() {
                    let args = call.args.to_string();
                    chunks.push(MockModelClient::tool_call(
                        index as u32,
                        Some(&call.id),
                        Some(&call.name),
                        &args,
                    ));
                }
                chunks.push(if turn.tools.is_empty() {
                    MockModelClient::finish_stop()
                } else {
                    MockModelClient::finish_tool_calls()
                });
                chunks
            })
            .collect()
    }

    /// A scripted [`ModelClient`] that replays this scenario's turns.
    pub fn model_client(&self) -> Arc<dyn ModelClient> {
        Arc::new(MockModelClient::new(self.chunks()))
    }

    /// The scripted room for this scenario, if it declares one — ready to hand to
    /// [`Session::with_room`](crate::Session::with_room).
    pub fn scripted_room(&self) -> Option<ScriptedRoom> {
        let room = self.room.as_ref()?;
        let incoming = room
            .incoming
            .iter()
            .map(|entry| ScriptedIncoming {
                message: RoomMessage {
                    from: entry.from.clone(),
                    subject: entry.subject.clone(),
                    body: entry.body.clone(),
                },
                after: Duration::from_millis(entry.after_ms),
            })
            .collect();
        Some(ScriptedRoom::new(room.roster.clone(), incoming))
    }

    /// A single-role [`ProviderPool`] backed by this scenario's scripted client, so a real
    /// [`Session`](crate::Session) can run the scenario through its normal loop.
    pub fn provider(&self) -> ProviderPool {
        ProviderPool::single(ResolvedRole {
            client: self.model_client(),
            model: SCRIPTED_MODEL.into(),
            temperature: None,
            max_tokens: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopping_turn_ends_with_finish_stop() {
        let scenario = Scenario {
            system: None,
            user: "hi".into(),
            turns: vec![ScriptTurn {
                text: Some("done".into()),
                tools: Vec::new(),
            }],
            room: None,
        };
        let chunks = scenario.chunks();
        assert_eq!(chunks.len(), 1);
        // One text delta plus one terminal finish chunk, and no tool-call fragments. ~keep
        assert_eq!(chunks[0].len(), 2);
    }

    #[test]
    fn a_tool_turn_emits_a_tool_fragment_and_finish_tool_calls() {
        let scenario = Scenario {
            system: None,
            user: "run it".into(),
            turns: vec![ScriptTurn {
                text: None,
                tools: vec![ScriptToolCall {
                    id: "c1".into(),
                    name: "shell:exec".into(),
                    args: serde_json::json!({ "command": "echo hi" }),
                }],
            }],
            room: None,
        };
        let chunks = scenario.chunks();
        // No text => just the tool-call fragment plus the terminal finish chunk. ~keep
        assert_eq!(chunks[0].len(), 2);
    }

    #[test]
    fn from_json_round_trips_a_scenario() {
        let scenario = Scenario::from_json(
            r#"{ "user": "u", "turns": [ { "text": "t", "tools": [ { "id": "c1",
                "name": "code:outline", "args": { "path": "src/lib.rs" } } ] } ] }"#,
        )
        .expect("parses");
        assert_eq!(scenario.user, "u");
        assert_eq!(scenario.turns.len(), 1);
        assert_eq!(scenario.turns[0].tools[0].name, "code:outline");
    }

    #[test]
    fn a_scenario_without_a_room_has_no_scripted_room() {
        let scenario = Scenario::from_json(r#"{ "user": "u", "turns": [ { "text": "t" } ] }"#).expect("parses");
        assert!(scenario.scripted_room().is_none());
    }

    #[tokio::test]
    async fn a_room_script_builds_a_scripted_room_with_roster_and_incoming() {
        use crate::room::RoomClient;

        let scenario = Scenario::from_json(
            r#"{ "user": "u", "turns": [ { "text": "t" } ], "room": {
                "roster": [ { "id": "alice", "display": "alice" } ],
                "incoming": [ { "from": "alice", "subject": "sync", "body": "ROOM-IN-42", "after_ms": 20 } ] } }"#,
        )
        .expect("parses");
        let room = scenario.scripted_room().expect("a room is declared");
        assert_eq!(
            room.roster().await.expect("roster"),
            vec![RoomPeer {
                id: "alice".into(),
                display: "alice".into(),
            }]
        );
        let history = room.history(None).await.expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].body, "ROOM-IN-42");
    }
}
