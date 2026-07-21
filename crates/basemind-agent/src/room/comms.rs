//! The real [`RoomClient`]: a [`CommsRoom`] backed by basemind's comms broker (feature `comms`).
//!
//! Both sides of the seam map onto [`CommsClient`](basemind::comms::client::CommsClient):
//! [`roster`](RoomClient::roster) / [`post`](RoomClient::post) / [`history`](RoomClient::history)
//! run on a shared request client behind an [`Arc<Mutex<_>>`](std::sync::Arc) (every `CommsClient`
//! call is `&mut self`, but the trait methods take `&self`), while
//! [`spawn_incoming`](RoomClient::spawn_incoming) opens its OWN dedicated connection so the blocking
//! `wait_inbox` long-poll never contends with request traffic on the shared client.
//!
//! The room itself is one broker [`Thread`](basemind::comms::model::Thread) addressed by the fixed
//! subject `"agent-room"` plus the repo path — two dimensions, satisfying the broker's "at least
//! two of subject / path / members" rule.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use basemind::comms::client::{CommsClient, scope_context_for};
use basemind::comms::identity::cli_agent_id;
use basemind::comms::ids::{AgentId, ThreadId};
use basemind::comms::model::{AgentCard, AgentRecord, now_micros};
use basemind::comms::protocol::SeqMeta;
use tokio::sync::{Mutex, broadcast};

use super::{RoomClient, RoomMessage, RoomPeer};
use crate::error::{AgentError, Result};
use crate::event::AgentEvent;

/// The fixed subject that names the per-repo agent room.
const ROOM_SUBJECT: &str = "agent-room";

/// Page size for a one-shot history read.
const HISTORY_LIMIT: u32 = 50;

/// Page size for each inbox long-poll.
const INBOX_LIMIT: u32 = 50;

/// How long each `wait_inbox` blocks before it loops and re-polls.
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// A [`RoomClient`] over basemind's comms broker: one shared request client plus a per-repo room
/// thread. The incoming stream runs on its own connection (see [`CommsRoom::spawn_incoming`]).
pub struct CommsRoom {
    agent: AgentId,
    remote: Option<String>,
    cwd: Option<PathBuf>,
    thread: ThreadId,
    client: Arc<Mutex<CommsClient>>,
}

impl CommsRoom {
    /// Connect to the broker for `root`: resolve this agent's identity and scope, spawn/attach the
    /// daemon, register the agent's card, then ensure the per-repo room thread exists and is joined.
    pub async fn connect(root: &Path) -> Result<Self> {
        let agent = cli_agent_id(root);
        let (remote, cwd) = scope_context_for(root);
        let mut client = CommsClient::ensure_and_connect(agent.clone(), remote.clone(), cwd.clone())
            .await
            .map_err(|error| AgentError::Tool(format!("room connect: {error}")))?;

        let card = AgentCard {
            name: agent.as_str().to_string(),
            description: "basemind agent".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            skills: Vec::new(),
        };
        // A re-register is last-writer-wins on the card, so a failure here is harmless — never fail ~keep
        // room bring-up on it. ~keep
        let _ = client.register_agent(card).await;

        let thread = Self::ensure_room_thread(&mut client, root, remote.clone(), cwd.clone()).await?;

        Ok(Self {
            agent,
            remote,
            cwd,
            thread,
            client: Arc::new(Mutex::new(client)),
        })
    }

    /// Find the existing `agent-room` thread (joining it) or start a fresh one addressed by subject
    /// + repo path.
    async fn ensure_room_thread(
        client: &mut CommsClient,
        root: &Path,
        remote: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Result<ThreadId> {
        let existing = client
            .list_threads(remote, cwd, Some(ROOM_SUBJECT.to_string()), false)
            .await
            .map_err(|error| AgentError::Tool(format!("room list_threads: {error}")))?;

        if let Some(thread) = existing
            .into_iter()
            .find(|t| t.subject.as_deref() == Some(ROOM_SUBJECT))
        {
            client
                .join_thread(thread.id.clone())
                .await
                .map_err(|error| AgentError::Tool(format!("room join_thread: {error}")))?;
            return Ok(thread.id);
        }

        let thread = client
            .start_thread(
                Some(ROOM_SUBJECT.to_string()),
                Some(root.display().to_string()),
                Vec::new(),
            )
            .await
            .map_err(|error| AgentError::Tool(format!("room start_thread: {error}")))?;
        Ok(thread.id)
    }
}

#[async_trait]
impl RoomClient for CommsRoom {
    async fn roster(&self) -> Result<Vec<RoomPeer>> {
        let mut client = self.client.lock().await;
        let records = client
            .list_agents(Some(self.thread.clone()))
            .await
            .map_err(|error| AgentError::Tool(format!("room roster: {error}")))?;
        Ok(records.into_iter().map(peer_from_record).collect())
    }

    async fn post(&self, subject: Option<String>, text: String) -> Result<()> {
        let mut client = self.client.lock().await;
        client
            .post_message(
                self.thread.clone(),
                subject.unwrap_or_else(|| "room".to_string()),
                text.into_bytes(),
                Vec::new(),
                None,
            )
            .await
            .map_err(|error| AgentError::Tool(format!("room post: {error}")))?;
        Ok(())
    }

    async fn history(&self, since_micros: Option<i64>) -> Result<Vec<RoomMessage>> {
        let mut client = self.client.lock().await;
        let (rows, _next) = client
            .read_history(self.thread.clone(), None, HISTORY_LIMIT, since_micros)
            .await
            .map_err(|error| AgentError::Tool(format!("room history: {error}")))?;
        let mut messages = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(body) = fetch_body(&mut client, &row).await {
                messages.push(message_from_row(&row, body));
            }
        }
        Ok(messages)
    }

    fn spawn_incoming(&self, events: broadcast::Sender<AgentEvent>) {
        let agent = self.agent.clone();
        let remote = self.remote.clone();
        let cwd = self.cwd.clone();
        let thread = self.thread.clone();
        tokio::spawn(async move {
            // A dedicated connection: `wait_inbox` blocks for up to WAIT_TIMEOUT, so sharing the ~keep
            // request client would stall every roster/post for the length of the long-poll. ~keep
            let mut client = match CommsClient::ensure_and_connect(agent.clone(), remote.clone(), cwd.clone()).await {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!(%error, "room incoming: connect failed");
                    return;
                }
            };
            if let Err(error) = client.join_thread(thread.clone()).await {
                tracing::warn!(%error, "room incoming: join failed");
                return;
            }

            match client.list_agents(Some(thread.clone())).await {
                Ok(records) => {
                    let peers = records.into_iter().map(peer_from_record).collect();
                    if events.send(AgentEvent::RoomRoster { peers }).is_err() {
                        return;
                    }
                }
                Err(error) => tracing::warn!(%error, "room incoming: roster failed"),
            }

            // Start from now: only NEW posts stream onto the event bus (the engine fetches backlog ~keep
            // via `history`). Advance past every row seen — including our own echoes — so a message ~keep
            // is never re-delivered and own-posts never wake us in a tight loop. ~keep
            let mut since = now_micros();
            loop {
                let rows = match client
                    .wait_inbox(
                        remote.clone(),
                        cwd.clone(),
                        Some(thread.clone()),
                        Some(since),
                        None,
                        INBOX_LIMIT,
                        WAIT_TIMEOUT,
                    )
                    .await
                {
                    Ok((_timed_out, rows, _unread, _cursor)) => rows,
                    Err(error) => {
                        tracing::warn!(%error, "room incoming: wait_inbox failed");
                        break;
                    }
                };
                for row in rows {
                    since = since.max(row.meta.ts_micros + 1);
                    if row.meta.from == agent {
                        continue;
                    }
                    let Some(body) = fetch_body(&mut client, &row).await else {
                        continue;
                    };
                    if events
                        .send(AgentEvent::RoomMessage(message_from_row(&row, body)))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }
}

/// Map a broker [`AgentRecord`] to a roster [`RoomPeer`], falling back to the id when the card has
/// no name.
fn peer_from_record(record: AgentRecord) -> RoomPeer {
    let id = record.agent_id.as_str().to_string();
    let display = if record.card.name.is_empty() {
        id.clone()
    } else {
        record.card.name
    };
    RoomPeer { id, display }
}

/// Build a [`RoomMessage`] from a front-matter row and its already-decoded body.
fn message_from_row(row: &SeqMeta, body: String) -> RoomMessage {
    RoomMessage {
        from: row.meta.from.as_str().to_string(),
        subject: row.meta.subject.clone(),
        body,
    }
}

/// Fetch and lossily-decode a message body, yielding `None` when the body is absent or the fetch
/// fails (a missing body is a row we simply skip, not a hard error).
async fn fetch_body(client: &mut CommsClient, row: &SeqMeta) -> Option<String> {
    match client.get_body(row.meta.id.clone()).await {
        Ok(Some(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "room: get_body failed");
            None
        }
    }
}
