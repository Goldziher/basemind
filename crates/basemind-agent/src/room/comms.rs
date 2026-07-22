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
use basemind::comms::singleton::{self, CommsPaths};
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

/// Cadence at which the incoming task re-polls the roster for join/leave deltas. Doubles as the
/// per-poll `wait_inbox` bound so a quiet room still refreshes the roster on this beat.
const ROSTER_REFRESH: Duration = Duration::from_secs(2);

/// First reconnect delay after the incoming stream drops; grows to [`RECONNECT_BACKOFF_MAX`].
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(50);

/// Cap on the exponential reconnect backoff — a persistently-dead broker retries at this ceiling.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Upper bound on the already-delivered message-id set the incoming task keeps for dedup. Evicts
/// oldest-first once full; only the newest microsecond's worth of ids is ever re-queried, so this
/// is comfortably larger than it needs to be.
const SEEN_IDS_CAP: usize = 4096;

/// A [`RoomClient`] over basemind's comms broker: one shared request client plus a per-repo room
/// thread. The incoming stream runs on its own connection (see [`CommsRoom::spawn_incoming`]).
pub struct CommsRoom {
    agent: AgentId,
    remote: Option<String>,
    cwd: Option<PathBuf>,
    paths: CommsPaths,
    thread: ThreadId,
    client: Arc<Mutex<CommsClient>>,
}

impl CommsRoom {
    /// Connect to the per-user singleton broker for `root`: resolve this agent's identity, scope,
    /// and endpoint, spawn/attach the daemon, then finish bring-up (register card + ensure room
    /// thread). The production entry point.
    pub async fn connect(root: &Path) -> Result<Self> {
        let paths =
            singleton::resolve_paths().map_err(|error| AgentError::Tool(format!("room resolve paths: {error}")))?;
        let agent = cli_agent_id(root);
        let (remote, cwd) = scope_context_for(root);
        singleton::ensure_daemon(&paths)
            .await
            .map_err(|error| AgentError::Tool(format!("room ensure daemon: {error}")))?;
        let client = CommsClient::connect(&paths, agent.clone(), remote.clone(), cwd.clone())
            .await
            .map_err(|error| AgentError::Tool(format!("room connect: {error}")))?;
        Self::finish_connect(root, paths, agent, remote, cwd, client).await
    }

    /// Injection seam: connect to an explicitly-provided broker endpoint and identity instead of
    /// the per-user singleton. Two rooms over one repo root but distinct [`AgentId`]s become two
    /// peers of the same `agent-room` thread — the shape a hermetic test drives against an isolated
    /// broker. Unlike [`connect`](Self::connect), the broker must already be running; this never
    /// spawns a daemon.
    pub async fn connect_with_paths(root: &Path, paths: &CommsPaths, agent: AgentId) -> Result<Self> {
        let (remote, cwd) = scope_context_for(root);
        let client = CommsClient::connect(paths, agent.clone(), remote.clone(), cwd.clone())
            .await
            .map_err(|error| AgentError::Tool(format!("room connect: {error}")))?;
        Self::finish_connect(root, paths.clone(), agent, remote, cwd, client).await
    }

    /// Shared bring-up once a request client is connected: register the agent card (best-effort,
    /// last-writer-wins) and ensure the per-repo room thread exists and is joined.
    async fn finish_connect(
        root: &Path,
        paths: CommsPaths,
        agent: AgentId,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        mut client: CommsClient,
    ) -> Result<Self> {
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
            paths,
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
        // Include archived threads: if the room was archived (idle-TTL sweep or a peer's ~keep
        // `leave` cascading to an archive), rejoin it rather than silently forking a duplicate ~keep
        // `agent-room`. Prefer an active match, falling back to an archived one. ~keep
        let existing = client
            .list_threads(remote, cwd, Some(ROOM_SUBJECT.to_string()), true)
            .await
            .map_err(|error| AgentError::Tool(format!("room list_threads: {error}")))?;

        let matching: Vec<_> = existing
            .into_iter()
            .filter(|t| t.subject.as_deref() == Some(ROOM_SUBJECT))
            .collect();
        if let Some(thread) = matching.iter().find(|t| t.active).or_else(|| matching.first()) {
            client
                .join_thread(thread.id.clone())
                .await
                .map_err(|error| AgentError::Tool(format!("room join_thread: {error}")))?;
            return Ok(thread.id.clone());
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
        let paths = self.paths.clone();
        let thread = self.thread.clone();
        tokio::spawn(incoming_task(paths, agent, remote, cwd, thread, events));
    }

    async fn leave(&self) -> Result<()> {
        let mut client = self.client.lock().await;
        client
            .leave_thread(self.thread.clone())
            .await
            .map_err(|error| AgentError::Tool(format!("room leave: {error}")))?;
        Ok(())
    }
}

/// Why the incoming stream stopped: the broadcast closed (session ended → the task returns), or a
/// transient transport error dropped the stream (reconnect with backoff and resume).
enum StreamExit {
    /// The event broadcast closed — the session ended, so the incoming task should return.
    Closed,
    /// A transient error dropped the stream — reconnect and resume from the last `since`.
    Reconnect,
}

/// A bounded, insertion-ordered set of already-delivered message ids. Backs the incoming stream's
/// dedup: [`insert`](Self::insert) reports whether an id is new, evicting the oldest id once the set
/// reaches its cap so it can never grow without bound.
struct SeenIds {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
    cap: usize,
}

impl SeenIds {
    /// A fresh dedup set bounded to `cap` ids.
    fn new(cap: usize) -> Self {
        Self {
            set: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
            cap,
        }
    }

    /// Record `id` as delivered. Returns `true` when it was newly inserted (i.e. not seen before),
    /// `false` when it was already present. Evicts the oldest id once the set is full.
    fn insert(&mut self, id: String) -> bool {
        if self.set.contains(&id) {
            return false;
        }
        if self.order.len() >= self.cap
            && let Some(oldest) = self.order.pop_front()
        {
            self.set.remove(&oldest);
        }
        self.order.push_back(id.clone());
        self.set.insert(id);
        true
    }
}

/// The detached incoming task: connect → join → stream, wrapped in a capped-exponential reconnect
/// loop so a transient transport error never kills the stream. `since` and the dedup set persist
/// across reconnects, so resuming re-delivers nothing and misses nothing.
///
/// Reconnect re-dials the injected endpoint via [`CommsClient::connect`] — never
/// [`singleton::ensure_daemon`], which would spawn the per-user daemon and break the hermetic seam.
/// In production the request client's own transparent reconnect resurrects a dead singleton; this
/// loop then re-dials the revived endpoint.
async fn incoming_task(
    paths: CommsPaths,
    agent: AgentId,
    remote: Option<String>,
    cwd: Option<PathBuf>,
    thread: ThreadId,
    events: broadcast::Sender<AgentEvent>,
) {
    // Start from now: only NEW posts stream onto the event bus (the engine fetches backlog via ~keep
    // `history`). ~keep
    let mut since = now_micros();
    let mut seen = SeenIds::new(SEEN_IDS_CAP);
    let mut last_peers: Option<Vec<RoomPeer>> = None;
    let mut backoff = RECONNECT_BACKOFF_INITIAL;
    loop {
        let connect =
            CommsClient::connect_with_respawn(&paths, agent.clone(), remote.clone(), cwd.clone(), never_spawn_daemon);
        let mut client = match connect.await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "room incoming: connect failed; backing off");
                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff);
                continue;
            }
        };
        if let Err(error) = client.join_thread(thread.clone()).await {
            tracing::warn!(%error, "room incoming: join failed; backing off");
            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff);
            continue;
        }
        // A live connection resets the backoff, so a single blip recovers on the short delay. ~keep
        backoff = RECONNECT_BACKOFF_INITIAL;
        match run_incoming_stream(
            &mut client,
            &agent,
            &remote,
            &cwd,
            &thread,
            &mut since,
            &mut seen,
            &mut last_peers,
            &events,
        )
        .await
        {
            StreamExit::Closed => return,
            StreamExit::Reconnect => {
                tracing::warn!("room incoming: stream dropped; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff);
            }
        }
    }
}

/// Double the reconnect delay, saturating at [`RECONNECT_BACKOFF_MAX`].
fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(RECONNECT_BACKOFF_MAX)
}

/// Respawn strategy for the dedicated incoming client: never spawn a daemon. Resurrecting the
/// per-user singleton is the request client's job — doing it here would race a second daemon into
/// existence (and break the hermetic seam, which points this client at an injected broker). Failing
/// fast instead lets [`incoming_task`]'s own reconnect loop re-dial the same endpoint once the
/// request client (or a test harness) has brought the broker back.
fn never_spawn_daemon(paths: &CommsPaths) -> std::io::Result<()> {
    Err(std::io::Error::other(format!(
        "incoming stream does not spawn a daemon for {}",
        paths.socket_path.display()
    )))
}

/// Stream a single live connection: interleave a roster refresh (on the [`ROSTER_REFRESH`] beat)
/// with a bounded `wait_inbox` long-poll, emitting roster deltas and per-message events. Returns
/// when the broadcast closes ([`StreamExit::Closed`]) or a transport error drops the stream
/// ([`StreamExit::Reconnect`]); the shared `since` / `seen` / `last_peers` carry across reconnects.
// The per-connection args plus the three cross-reconnect `&mut` cursors (since / seen / last_peers, ~keep
// owned by `incoming_task` so they survive a reconnect) genuinely belong together; bundling them ~keep
// into a struct of `&mut` refs would not reduce the coupling, only rename it. ~keep
#[allow(clippy::too_many_arguments)]
async fn run_incoming_stream(
    client: &mut CommsClient,
    agent: &AgentId,
    remote: &Option<String>,
    cwd: &Option<PathBuf>,
    thread: &ThreadId,
    since: &mut i64,
    seen: &mut SeenIds,
    last_peers: &mut Option<Vec<RoomPeer>>,
    events: &broadcast::Sender<AgentEvent>,
) -> StreamExit {
    // `None` forces the first roster poll immediately on (re)connect. ~keep
    let mut last_roster_at: Option<std::time::Instant> = None;
    loop {
        let roster_due = last_roster_at.map(|at| at.elapsed() >= ROSTER_REFRESH).unwrap_or(true);
        if roster_due {
            match client.list_agents(Some(thread.clone())).await {
                Ok(records) => {
                    if emit_roster_change(records, last_peers, events).is_err() {
                        return StreamExit::Closed;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "room incoming: roster poll failed");
                    return StreamExit::Reconnect;
                }
            }
            last_roster_at = Some(std::time::Instant::now());
        }

        let rows = match client
            .wait_inbox(
                remote.clone(),
                cwd.clone(),
                Some(thread.clone()),
                Some(*since),
                None,
                INBOX_LIMIT,
                ROSTER_REFRESH,
            )
            .await
        {
            Ok((_timed_out, rows, _unread, _cursor)) => rows,
            Err(error) => {
                tracing::warn!(%error, "room incoming: wait_inbox failed");
                return StreamExit::Reconnect;
            }
        };

        // Query `since` inclusively so a message sharing a microsecond with one already delivered ~keep
        // still surfaces; the id-set skips the re-read of the boundary microsecond. Only once a ~keep
        // batch delivers nothing new do we step `since` past it, so the next poll blocks (a live ~keep
        // read that always returned the boundary rows would spin) instead of re-scanning. ~keep
        let batch_empty = rows.is_empty();
        let mut max_ts = *since;
        let mut delivered_new = false;
        for row in rows {
            max_ts = max_ts.max(row.meta.ts_micros);
            if row.meta.from == *agent {
                continue;
            }
            if !seen.insert(row.meta.id.clone()) {
                continue;
            }
            let Some(body) = fetch_body(client, &row).await else {
                continue;
            };
            if events
                .send(AgentEvent::RoomMessage(message_from_row(&row, body)))
                .is_err()
            {
                return StreamExit::Closed;
            }
            delivered_new = true;
        }
        if delivered_new {
            *since = max_ts;
        } else if !batch_empty {
            *since = max_ts + 1;
        }
    }
}

/// Diff a fresh roster snapshot against the last one and, on any membership change, emit a refreshed
/// [`AgentEvent::RoomRoster`] plus an [`AgentEvent::RoomPeerJoined`] / [`AgentEvent::RoomPeerLeft`]
/// per delta. The first snapshot publishes only the full roster (no per-peer join). `Err(())` means
/// the broadcast closed — the caller ends the session.
fn emit_roster_change(
    records: Vec<AgentRecord>,
    last: &mut Option<Vec<RoomPeer>>,
    events: &broadcast::Sender<AgentEvent>,
) -> std::result::Result<(), ()> {
    let peers: Vec<RoomPeer> = records.into_iter().map(peer_from_record).collect();
    let now_ids: std::collections::HashSet<String> = peers.iter().map(|peer| peer.id.clone()).collect();
    match last {
        None => {
            events
                .send(AgentEvent::RoomRoster { peers: peers.clone() })
                .map_err(drop_err)?;
        }
        Some(prev) => {
            let prev_ids: std::collections::HashSet<String> = prev.iter().map(|peer| peer.id.clone()).collect();
            if prev_ids == now_ids {
                return Ok(());
            }
            for peer in &peers {
                if !prev_ids.contains(&peer.id) {
                    events
                        .send(AgentEvent::RoomPeerJoined { peer: peer.clone() })
                        .map_err(drop_err)?;
                }
            }
            for peer in prev.iter() {
                if !now_ids.contains(&peer.id) {
                    events
                        .send(AgentEvent::RoomPeerLeft { id: peer.id.clone() })
                        .map_err(drop_err)?;
                }
            }
            events
                .send(AgentEvent::RoomRoster { peers: peers.clone() })
                .map_err(drop_err)?;
        }
    }
    *last = Some(peers);
    Ok(())
}

/// Collapse a broadcast [`SendError`](broadcast::error::SendError) to `()`: the only failure a send
/// can report is a closed channel, which every caller treats identically (the session ended).
fn drop_err<T>(_error: broadcast::error::SendError<T>) {}

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
