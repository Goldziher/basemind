//! The broker: the single owner of all comms state.
//!
//! [`Broker`] wraps the [`CommsStore`] and an in-RAM registry of live notification sinks. It
//! handles each [`CommsRequest`] and fans out [`CommsNotification::Message`] to every link
//! subscribed to the posted room. The daemon is the sole writer to the store, so request
//! handling needs no cross-process coordination beyond the store's flock.
//!
//! ## Lifecycle
//!
//! `Starting → Active ⇄ Idle → Draining → Stopped`. The subscriber refcount drives the
//! Active⇄Idle edge: when it drops to zero (and a grace period elapses) the broker may shed
//! in-RAM caches and pause timers, but it KEEPS the socket bound and the store flock held —
//! that is the split-brain guard, so a second daemon cannot bind while this one merely idles.
//! `Draining` (a `Stop` RPC or SIGTERM) stops accepting, flushes, then releases the flock and
//! unlinks the socket on the way to `Stopped`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use super::cursor::Cursor;
use super::ids::{AgentId, RoomId};
use super::model::{
    AgentCard, AgentKind, AgentRecord, MessageBody, MessageMeta, Room, RoomScope, SessionLineage,
    Subscription, now_micros,
};
use super::protocol::{
    CommsNotification, CommsOut, CommsRequest, CommsResponse, PROTO_VER, SeqMeta, StatusReport,
};
use super::scope::{self, ScopeChain};
use super::store::{self, CommsStore, CommsStoreError};

/// Default page size when a client omits `limit`.
pub const DEFAULT_LIMIT: u32 = 100;
/// Hard cap on a page, mirroring the MCP `limit` ceiling.
pub const MAX_LIMIT: u32 = 1000;

/// Idle window after which a daemon with no connected links and no activity self-terminates.
/// Without this, daemons orphaned by a dead session (reparented to pid 1) linger forever and
/// pile up across sessions. The reaper in `cmd_comms_daemon` drives the normal drain path, so
/// the socket + flock are released cleanly on the way out. A live client (even a quiet
/// subscriber holding an open link) keeps the daemon alive; only a fully-unused daemon reaps.
pub const IDLE_REAP_AFTER: Duration = Duration::from_secs(30 * 60);
/// How often the idle reaper re-checks the broker. Small relative to [`IDLE_REAP_AFTER`].
pub const IDLE_REAP_CHECK_EVERY: Duration = Duration::from_secs(60);

/// Lifecycle state of the broker. See the module docs for the transition rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    /// Booting: store opening, front-ends not yet accepting.
    Starting,
    /// Serving with at least one live subscriber.
    Active,
    /// No live subscribers; socket + flock retained, caches may be shed.
    Idle,
    /// Stop requested: refusing new work, flushing.
    Draining,
    /// Fully stopped; flock released, socket unlinked.
    Stopped,
}

/// A registered notification sink for one subscription. The link's writer half drains it.
struct SubSink {
    /// The room this sink streams.
    room: RoomId,
    /// The agent owning the subscription. Retained for diagnostics + future per-agent
    /// targeting; the fan-out currently routes by room.
    #[allow(dead_code)]
    agent: AgentId,
    /// Where notifications are pushed.
    tx: mpsc::Sender<CommsOut>,
}

/// In-RAM broker state behind a single async mutex. Subscriber churn is low-frequency relative
/// to posts, so one mutex is simpler than sharding and never on a hot loop.
struct Registry {
    /// Live notification sinks keyed by subscription handle.
    sinks: AHashMap<u64, SubSink>,
    /// Current lifecycle state.
    state: LifecycleState,
}

/// The broker. Cheap to share via `Arc`; every front-end and link holds one.
pub struct Broker {
    store: Arc<CommsStore>,
    registry: Mutex<Registry>,
    /// Count of live notification subscribers; drives the Active⇄Idle edge.
    subscriber_count: AtomicUsize,
    /// Count of connected front-end links (clients). Drives the idle reaper: a daemon with
    /// zero links and no recent activity past [`IDLE_REAP_AFTER`] self-terminates.
    link_count: AtomicUsize,
    /// Millis since `started` at the last request / link connect / disconnect. Compared against
    /// [`IDLE_REAP_AFTER`] so a daemon serving frequent one-shot calls (which hold no long-lived
    /// subscriber) is not reaped mid-use.
    last_activity_ms: AtomicU64,
    /// Monotonic source of subscription handles.
    next_sub: AtomicU64,
    /// When the broker started, for uptime reporting.
    started: Instant,
    /// Build version string surfaced in `Welcome` / `Status`.
    version: String,
}

impl Broker {
    /// Construct a broker over an already-opened store.
    pub fn new(store: Arc<CommsStore>) -> Self {
        Self {
            store,
            registry: Mutex::new(Registry {
                sinks: AHashMap::new(),
                state: LifecycleState::Starting,
            }),
            subscriber_count: AtomicUsize::new(0),
            link_count: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
            next_sub: AtomicU64::new(1),
            started: Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Mark the broker Active once front-ends are accepting.
    pub async fn mark_active(&self) {
        let mut reg = self.registry.lock().await;
        if reg.state == LifecycleState::Starting || reg.state == LifecycleState::Idle {
            reg.state = LifecycleState::Active;
        }
    }

    /// Current live subscriber count.
    pub fn subscriber_count(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// Record a newly connected front-end link and stamp activity.
    pub fn link_connected(&self) {
        self.link_count.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    /// Record a front-end link closing and stamp activity.
    pub fn link_disconnected(&self) {
        self.link_count.fetch_sub(1, Ordering::Relaxed);
        self.touch();
    }

    /// Stamp "now" as the last-activity time. Called on every handled request and on link
    /// connect / disconnect, so the idle reaper measures from genuine quiescence.
    pub fn touch(&self) {
        self.last_activity_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// True when the broker has no connected links and no activity within `idle_for` — the
    /// signal for the daemon to self-terminate. Never true while draining or stopped, and never
    /// while a client link is open (a quiet subscriber must not be reaped out from under itself).
    pub async fn is_idle_for(&self, idle_for: Duration) -> bool {
        if self.link_count.load(Ordering::Relaxed) != 0 {
            return false;
        }
        if matches!(
            self.state().await,
            LifecycleState::Draining | LifecycleState::Stopped
        ) {
            return false;
        }
        let now_ms = self.started.elapsed().as_millis() as u64;
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        now_ms.saturating_sub(last) >= idle_for.as_millis() as u64
    }

    /// Handle one request on a link. `link_tx` is the link's outbound sink, used both for the
    /// direct response (returned) and to register notification streams. `agent`/`chain` are
    /// the per-link session context established by `Hello`. Returns the direct response, or
    /// `None` for fire-and-forget requests (`Subscribe`/`Unsubscribe` reply through `link_tx`).
    pub async fn handle(
        &self,
        req: CommsRequest,
        session: &mut Session,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> CommsResponse {
        self.touch();
        match self.dispatch(req, session, link_tx).await {
            Ok(resp) => resp,
            Err(e) => CommsResponse::Error {
                code: "store_error".to_string(),
                message: e.to_string(),
            },
        }
    }

    async fn dispatch(
        &self,
        req: CommsRequest,
        session: &mut Session,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> Result<CommsResponse, CommsStoreError> {
        match req {
            CommsRequest::Hello {
                agent,
                proto_ver,
                remote,
                cwd,
                session_id,
                parent_agent,
            } => {
                self.on_hello(
                    agent,
                    proto_ver,
                    remote,
                    cwd,
                    session_id,
                    parent_agent,
                    session,
                )
                .await
            }
            CommsRequest::Register { card } => self.on_register(session, card),
            CommsRequest::ListAgents { room } => self.on_list_agents(room),
            CommsRequest::CreateRoom { room, scope, title } => {
                self.on_create_room(room, scope, title)
            }
            CommsRequest::ListRooms { remote, cwd } => {
                self.on_list_rooms(remote, cwd, session).await
            }
            CommsRequest::Join { room } => self.on_join(session, room),
            CommsRequest::Leave { room } => self.on_leave(session, room),
            CommsRequest::Post {
                room,
                subject,
                tags,
                reply_to,
                scope,
                body,
            } => {
                self.on_post(session, room, subject, tags, reply_to, scope, body)
                    .await
            }
            CommsRequest::History {
                room,
                cursor,
                limit,
            } => self.on_history(room, cursor, limit),
            CommsRequest::GetBody { message_id } => self.on_get_body(message_id),
            CommsRequest::Inbox {
                remote,
                cwd,
                cursor,
                limit,
                mark_read,
            } => {
                self.on_inbox(session, remote, cwd, cursor, limit, mark_read)
                    .await
            }
            CommsRequest::AckInbox {
                message_ids,
                room,
                to_seq,
            } => self.on_ack(session, message_ids, room, to_seq),
            CommsRequest::Subscribe { room } => self.on_subscribe(session, room, link_tx).await,
            CommsRequest::Unsubscribe { sub } => self.on_unsubscribe(sub).await,
            CommsRequest::ListSessions {} => self.on_list_sessions(),
            CommsRequest::Ping => Ok(CommsResponse::Pong),
            CommsRequest::Status => Ok(self.on_status().await),
            CommsRequest::Stop => {
                self.begin_drain().await;
                Ok(CommsResponse::Ok)
            }
        }
    }

    // ─── handlers ─────────────────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    async fn on_hello(
        &self,
        agent: AgentId,
        proto_ver: u32,
        remote: Option<String>,
        cwd: Option<std::path::PathBuf>,
        session_id: Option<String>,
        parent_agent: Option<String>,
        session: &mut Session,
    ) -> Result<CommsResponse, CommsStoreError> {
        if proto_ver != PROTO_VER {
            return Ok(CommsResponse::Error {
                code: "proto_skew".to_string(),
                message: format!("daemon speaks proto {PROTO_VER}, client sent {proto_ver}"),
            });
        }
        session.agent = Some(agent.clone());
        // A malformed parent id from the wire is dropped rather than rejecting the Hello — the
        // lineage hint is advisory; the session room match keys on `session_id` alone.
        let parent_agent = parent_agent.and_then(|p| AgentId::parse(p).ok());
        session.session_id = session_id.clone();
        session.parent_agent = parent_agent.clone();
        let mut chain = build_chain(remote.clone(), cwd.clone());
        chain.session_id = session_id;
        chain.parent_agent = parent_agent;
        session.chain = Some(chain);

        // Record / refresh the agent.
        let now = now_micros();
        let record = match self.store.get_agent(&agent)? {
            Some(mut existing) => {
                existing.last_seen = now;
                existing
            }
            None => AgentRecord {
                agent_id: agent.clone(),
                card: AgentCard::default(),
                kind: AgentKind::Other,
                first_seen: now,
                last_seen: now,
            },
        };
        self.store.put_agent(&record)?;

        // Auto-join every scope-matching room and the default per-repo room.
        if let Some(chain) = session.chain.clone() {
            let session_room = self.auto_join(&agent, &chain)?;
            // Persist the session lineage row now that the child is joined to its session room.
            // Best-effort: a store failure here must not fail the handshake.
            if let Err(e) = self.record_session_lineage(&agent, &chain, session_room) {
                tracing::warn!(error = %e, "comms: failed to record session lineage");
            }
        }

        Ok(CommsResponse::Welcome {
            proto_ver: PROTO_VER,
            daemon_version: self.version.clone(),
        })
    }

    /// Persist a [`SessionLineage`] row at the child's `Hello`. No-op when the chain carries no
    /// `session_id` (a top-level agent). `session_room` is the room [`Self::auto_join`] just joined
    /// the child to — passing it in (rather than re-scanning) means the lineage points at the exact
    /// room the child joined and the room keyspace is scanned once per `Hello`. The `created_at` of
    /// an existing row is preserved across reconnects so a re-`Hello` does not rewrite first-seen.
    fn record_session_lineage(
        &self,
        agent: &AgentId,
        chain: &ScopeChain,
        session_room: Option<RoomId>,
    ) -> Result<(), CommsStoreError> {
        let Some(sid) = chain.session_id.clone() else {
            return Ok(());
        };
        let Some(room_id) = session_room else {
            // The child presented a session id but no `RoomScope::Session` room matched — the parent
            // has not created it yet (out-of-order startup). Skip the row; a later `Hello` (after the
            // room exists) records it. Warn so the ordering bug is diagnosable.
            tracing::warn!(
                session_id = %sid,
                agent = %agent,
                "comms: session id presented but no session room to anchor lineage; skipping"
            );
            return Ok(());
        };
        // Preserve the original first-seen time across reconnects.
        let created_at = match self.store.get_session(&sid)? {
            Some(existing) => existing.created_at,
            None => now_micros(),
        };
        self.store.put_session(&SessionLineage {
            session_id: sid,
            parent_agent: chain.parent_agent.clone(),
            child_agent: agent.clone(),
            room_id,
            created_at,
        })
    }

    fn on_list_sessions(&self) -> Result<CommsResponse, CommsStoreError> {
        Ok(CommsResponse::Sessions {
            sessions: self.store.list_sessions()?,
        })
    }

    fn on_register(
        &self,
        session: &Session,
        card: AgentCard,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let now = now_micros();
        let record = match self.store.get_agent(&agent)? {
            Some(mut existing) => {
                existing.card = card;
                existing.last_seen = now;
                existing
            }
            None => AgentRecord {
                agent_id: agent,
                card,
                kind: AgentKind::Other,
                first_seen: now,
                last_seen: now,
            },
        };
        self.store.put_agent(&record)?;
        Ok(CommsResponse::Ok)
    }

    fn on_list_agents(&self, room: Option<RoomId>) -> Result<CommsResponse, CommsStoreError> {
        let agents = match room {
            None => self.store.list_agents()?,
            Some(room) => {
                let subs = self.store.subscribers(&room)?;
                let mut out = Vec::new();
                for id in subs {
                    if let Some(rec) = self.store.get_agent(&id)? {
                        out.push(rec);
                    }
                }
                out
            }
        };
        Ok(CommsResponse::Agents(agents))
    }

    fn on_create_room(
        &self,
        room: RoomId,
        scope: RoomScope,
        title: Option<String>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let record = Room {
            room_id: room.clone(),
            scope,
            title: title.unwrap_or_else(|| room.as_str().to_string()),
            created_at: now_micros(),
        };
        self.store.put_room(&record)?;
        Ok(CommsResponse::Room(record))
    }

    async fn on_list_rooms(
        &self,
        remote: Option<String>,
        cwd: Option<std::path::PathBuf>,
        session: &mut Session,
    ) -> Result<CommsResponse, CommsStoreError> {
        let chain = build_chain(remote, cwd);
        // Auto-join the matching rooms for the session agent, if it has said Hello.
        if let Some(agent) = session.agent.clone() {
            self.auto_join(&agent, &chain)?;
        }
        let matching: Vec<Room> = self
            .store
            .list_rooms()?
            .into_iter()
            .filter(|r| scope::room_matches(&r.scope, &chain))
            .collect();
        Ok(CommsResponse::Rooms(matching))
    }

    fn on_join(&self, session: &Session, room: RoomId) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store.subscribe(&Subscription {
            agent_id: agent,
            room,
            created_at: now_micros(),
        })?;
        Ok(CommsResponse::Ok)
    }

    fn on_leave(&self, session: &Session, room: RoomId) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store.unsubscribe(&room, &agent)?;
        Ok(CommsResponse::Ok)
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_post(
        &self,
        session: &Session,
        room: RoomId,
        subject: String,
        tags: Vec<String>,
        reply_to: Option<String>,
        scope: Vec<String>,
        body: Vec<u8>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let id = mint_message_id(&room, &agent);
        let meta = store::build_meta(
            id,
            room.clone(),
            agent,
            subject,
            tags,
            reply_to,
            scope,
            &body,
        );
        let (_, stored) = self.store.post(&room, meta, MessageBody(body))?;
        self.fan_out(&room, &stored).await;
        Ok(CommsResponse::Posted {
            message_id: stored.id,
        })
    }

    fn on_history(
        &self,
        room: RoomId,
        cursor: Option<Cursor>,
        limit: Option<u32>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let after = decode_after(cursor.as_ref(), room.as_str());
        let limit = clamp_limit(limit);
        let page = self.store.history(&room, after, limit)?;
        let next = page
            .more
            .then(|| Cursor::encode(room.as_str(), page.last_seq));
        let messages = page
            .messages
            .into_iter()
            .map(|(seq, meta)| SeqMeta { seq, meta })
            .collect();
        Ok(CommsResponse::History {
            messages,
            next_cursor: next,
        })
    }

    fn on_get_body(&self, message_id: String) -> Result<CommsResponse, CommsStoreError> {
        let body = self.store.get_body(&message_id)?;
        Ok(CommsResponse::Body { body })
    }

    async fn on_inbox(
        &self,
        session: &mut Session,
        remote: Option<String>,
        cwd: Option<std::path::PathBuf>,
        cursor: Option<Cursor>,
        limit: Option<u32>,
        mark_read: bool,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let chain = build_chain(remote, cwd);
        self.auto_join(&agent, &chain)?;

        let limit = clamp_limit(limit);
        // Walk the agent's subscribed rooms in a stable (id-sorted) order, gathering messages
        // past each room's per-agent read cursor. The cursor token carries the room + seq of
        // where the previous page stopped so we resume mid-walk deterministically.
        let resume = cursor.as_ref().and_then(|c| c.decode().ok());
        let mut rooms = self.store.rooms_for_agent(&agent)?;
        rooms.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut collected: Vec<SeqMeta> = Vec::new();
        // Highest delivered seq per room in THIS page — used to mark-read and to mint the
        // next cursor.
        let mut delivered_high: Vec<(RoomId, u64)> = Vec::new();
        let mut unread_remaining: u32 = 0;
        let mut next_cursor: Option<Cursor> = None;

        for room in &rooms {
            let read_seq = self.store.read_cursor(&agent, room)?;
            // Resume point for THIS room: its read cursor, bumped past anything already paged
            // when the resume token points at this room.
            let after = match &resume {
                Some(pos) if pos.room == room.as_str() => pos.seq.max(read_seq),
                _ => read_seq,
            };
            // +1 so we can tell whether more remain past the page budget for this room.
            let remaining = limit.saturating_sub(collected.len());
            let want = remaining.saturating_add(1).max(1);
            let rows = self.store.history_with_seq(room, after, want)?;
            for (seq, meta) in rows {
                // The agent's own posts are not "inbox" for their author — skip them from the
                // page and the unread count, but still record the seq so `mark_read` advances the
                // read cursor past them (they must never resurface). `room_history` is unaffected:
                // the full log still shows self-authored messages.
                if meta.from == agent {
                    upsert_high(&mut delivered_high, room, seq);
                    continue;
                }
                if collected.len() < limit {
                    collected.push(SeqMeta { seq, meta });
                    upsert_high(&mut delivered_high, room, seq);
                } else {
                    // Overflow: this is where the next page resumes.
                    unread_remaining = unread_remaining.saturating_add(1);
                    if next_cursor.is_none() {
                        let resume_seq = highest_for(&delivered_high, room).unwrap_or(after);
                        next_cursor = Some(Cursor::encode(room.as_str(), resume_seq));
                    }
                }
            }
        }

        if mark_read {
            for (room, seq) in &delivered_high {
                self.store.set_read_cursor(&agent, room, *seq)?;
            }
        }

        Ok(CommsResponse::Inbox {
            messages: collected,
            unread: unread_remaining,
            next_cursor,
        })
    }

    /// Acknowledge inbox messages by advancing the calling agent's per-room read cursors. Never
    /// touches the shared log and never affects another agent: an ack is purely a per-agent cursor
    /// move. Supports both the `message_ids` mode (resolve each id → `(room, seq)`, advance each
    /// room to its max acked seq) and the bulk `room` + `to_seq` mode, applying both when given.
    fn on_ack(
        &self,
        session: &Session,
        message_ids: Vec<String>,
        room: Option<RoomId>,
        to_seq: Option<u64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let bulk = matches!((&room, to_seq), (Some(_), Some(_)));
        if message_ids.is_empty() && !bulk {
            return Ok(CommsResponse::Error {
                code: "empty_ack".to_string(),
                message: "ack requires message_ids or a (room, to_seq) pair".to_string(),
            });
        }

        // Accumulate the highest seq to advance per room across both modes, then apply once so a
        // room targeted by both modes advances to the single maximum.
        let mut targets: Vec<(RoomId, u64)> = Vec::new();
        let mut acked: u32 = 0;
        if !message_ids.is_empty() {
            for (_, room, seq) in self.store.resolve_ids(&message_ids)? {
                acked = acked.saturating_add(1);
                upsert_high(&mut targets, &room, seq);
            }
        }
        if let (Some(room), Some(seq)) = (room, to_seq) {
            upsert_high(&mut targets, &room, seq);
        }

        // Apply each advance (monotonic in the store) and report ONLY the rooms whose cursor
        // actually moved, so the response never claims a phantom advance for an already-acked seq
        // or a `to_seq` at or below the current position (e.g. `to_seq = 0`).
        let mut cursors_advanced: Vec<(String, u64)> = Vec::new();
        for (room, seq) in &targets {
            let before = self.store.read_cursor(&agent, room)?;
            self.store.set_read_cursor(&agent, room, *seq)?;
            let after = self.store.read_cursor(&agent, room)?;
            if after > before {
                cursors_advanced.push((room.as_str().to_string(), after));
            }
        }

        Ok(CommsResponse::Acked {
            acked,
            cursors_advanced,
        })
    }

    async fn on_subscribe(
        &self,
        session: &Session,
        room: RoomId,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        // Persist the subscription so the inbox sees it even across reconnects.
        self.store.subscribe(&Subscription {
            agent_id: agent.clone(),
            room: room.clone(),
            created_at: now_micros(),
        })?;
        let sub = self.next_sub.fetch_add(1, Ordering::Relaxed);
        {
            let mut reg = self.registry.lock().await;
            reg.sinks.insert(
                sub,
                SubSink {
                    room,
                    agent,
                    tx: link_tx.clone(),
                },
            );
            reg.state = LifecycleState::Active;
        }
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        Ok(CommsResponse::Subscribed { sub })
    }

    async fn on_unsubscribe(&self, sub: u64) -> Result<CommsResponse, CommsStoreError> {
        let removed = {
            let mut reg = self.registry.lock().await;
            reg.sinks.remove(&sub)
        };
        if removed.is_some() {
            self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
            self.maybe_idle().await;
        }
        Ok(CommsResponse::Ok)
    }

    async fn on_status(&self) -> CommsResponse {
        let rooms = self.store.list_rooms().map(|r| r.len()).unwrap_or(0);
        CommsResponse::Status(StatusReport {
            pid: std::process::id(),
            version: self.version.clone(),
            proto_ver: PROTO_VER,
            uptime_secs: self.started.elapsed().as_secs(),
            rooms: u32::try_from(rooms).unwrap_or(u32::MAX),
            subscribers: u32::try_from(self.subscriber_count()).unwrap_or(u32::MAX),
        })
    }

    // ─── internals ────────────────────────────────────────────────────────────────────────

    /// Subscribe `agent` to every registered room whose scope matches `chain`, auto-creating
    /// and registering a default room for the agent's repo/workspace on first sight. Logs each
    /// auto-join.
    /// Auto-join the agent to every scope-matching room (and the default per-repo room).
    ///
    /// Returns the id of the `RoomScope::Session(chain.session_id)` room the agent was joined to,
    /// if one matched — threaded into [`Self::record_session_lineage`] so the lineage row points at
    /// the exact room the child joined (not a re-scan that could pick a different room sharing the
    /// scope) and the room keyspace is scanned once per `Hello` rather than twice.
    fn auto_join(
        &self,
        agent: &AgentId,
        chain: &ScopeChain,
    ) -> Result<Option<RoomId>, CommsStoreError> {
        // Ensure a default room exists for this scope.
        let default = default_room_for(chain);
        if self.store.get_room(&default.room_id)?.is_none() {
            tracing::info!(
                room = %default.room_id,
                "comms: auto-creating default room for scope"
            );
            self.store.put_room(&default)?;
        }

        let mut session_room = None;
        for room in self.store.list_rooms()? {
            if scope::room_matches(&room.scope, chain) {
                if matches!(&room.scope, RoomScope::Session(_)) {
                    session_room = Some(room.room_id.clone());
                }
                let already = self
                    .store
                    .subscribers(&room.room_id)?
                    .iter()
                    .any(|a| a == agent);
                if !already {
                    tracing::info!(
                        agent = %agent,
                        room = %room.room_id,
                        "comms: auto-joining agent to scope-matching room"
                    );
                    self.store.subscribe(&Subscription {
                        agent_id: agent.clone(),
                        room: room.room_id.clone(),
                        created_at: now_micros(),
                    })?;
                }
            }
        }
        Ok(session_room)
    }

    /// Push a new message to every live sink subscribed to `room`. Best-effort: a sink whose
    /// channel is full or closed is dropped (the link's reader will clean up its own sinks).
    async fn fan_out(&self, room: &RoomId, meta: &MessageMeta) {
        let mut dead: Vec<u64> = Vec::new();
        {
            let reg = self.registry.lock().await;
            for (sub, sink) in reg.sinks.iter() {
                if &sink.room == room {
                    let note = CommsOut::Notification(CommsNotification::Message(meta.clone()));
                    if sink.tx.try_send(note).is_err() {
                        dead.push(*sub);
                    }
                }
            }
        }
        if !dead.is_empty() {
            let mut reg = self.registry.lock().await;
            for sub in dead {
                if reg.sinks.remove(&sub).is_some() {
                    self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Transition to Idle when the last subscriber leaves. Keeps the socket + flock; only
    /// sheds the (currently negligible) in-RAM caches.
    async fn maybe_idle(&self) {
        if self.subscriber_count() == 0 {
            let mut reg = self.registry.lock().await;
            if reg.state == LifecycleState::Active {
                reg.state = LifecycleState::Idle;
                tracing::debug!("comms: broker idle (no subscribers); socket + flock retained");
            }
        }
    }

    /// Enter the Draining state and notify every live sink to disconnect. The front-end's
    /// shutdown watch is what actually stops the accept loop and unbinds the socket.
    pub async fn begin_drain(&self) {
        let sinks: Vec<mpsc::Sender<CommsOut>> = {
            let mut reg = self.registry.lock().await;
            reg.state = LifecycleState::Draining;
            reg.sinks.values().map(|s| s.tx.clone()).collect()
        };
        for tx in sinks {
            let _ = tx
                .send(CommsOut::Notification(CommsNotification::Shutdown))
                .await;
        }
    }

    /// Current lifecycle state.
    pub async fn state(&self) -> LifecycleState {
        self.registry.lock().await.state
    }
}

/// Per-link session context. Established by `Hello`, then read by every subsequent handler on
/// that link.
#[derive(Default)]
pub struct Session {
    /// The authenticated agent id for this link.
    pub agent: Option<AgentId>,
    /// The scope chain captured at Hello, used for auto-join.
    pub chain: Option<ScopeChain>,
    /// The terminal session id presented at Hello, if any. Drives session-scoped auto-join.
    pub session_id: Option<String>,
    /// The agent that spawned this one, captured at Hello for lineage bookkeeping.
    pub parent_agent: Option<AgentId>,
}

fn need_hello() -> CommsResponse {
    CommsResponse::Error {
        code: "no_hello".to_string(),
        message: "send Hello before any other request".to_string(),
    }
}

fn clamp_limit(limit: Option<u32>) -> usize {
    usize::try_from(limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT as usize)
}

fn decode_after(cursor: Option<&Cursor>, room: &str) -> u64 {
    match cursor.and_then(|c| c.decode().ok()) {
        Some(pos) if pos.room == room || pos.room.is_empty() => pos.seq,
        _ => 0,
    }
}

/// Record the highest delivered `seq` for `room` in a small per-page accumulator.
fn upsert_high(acc: &mut Vec<(RoomId, u64)>, room: &RoomId, seq: u64) {
    if let Some(entry) = acc.iter_mut().find(|(r, _)| r == room) {
        if seq > entry.1 {
            entry.1 = seq;
        }
    } else {
        acc.push((room.clone(), seq));
    }
}

/// Look up the highest delivered `seq` recorded for `room`.
fn highest_for(acc: &[(RoomId, u64)], room: &RoomId) -> Option<u64> {
    acc.iter().find(|(r, _)| r == room).map(|(_, s)| *s)
}

/// Build a scope chain from the optional remote + cwd a client supplied. When `cwd` is given
/// we attempt git discovery to enrich the chain's remote if the client did not supply one.
fn build_chain(remote: Option<String>, cwd: Option<std::path::PathBuf>) -> ScopeChain {
    match cwd {
        Some(cwd) => {
            let repo = crate::git::Repo::discover(&cwd).ok();
            let mut chain = scope::scope_chain(&cwd, repo.as_ref());
            if chain.remote.is_none() {
                chain.remote = remote;
            }
            chain
        }
        None => ScopeChain {
            remote,
            cwd: std::path::PathBuf::new(),
            ancestors: Vec::new(),
            // Session context is layered on by `on_hello` after the base chain is built.
            session_id: None,
            parent_agent: None,
        },
    }
}

/// The default room every agent in a scope auto-joins on first sight. Keyed by remote when the
/// agent is in a repo with a remote, else by the repo/workspace path, else Global.
fn default_room_for(chain: &ScopeChain) -> Room {
    let (room_id, scope, title) = match (&chain.remote, chain.cwd.as_os_str().is_empty()) {
        (Some(remote), _) => (
            RoomId::parse(sanitize_id(remote)).unwrap_or_else(|_| fallback_room()),
            RoomScope::Remote(remote.clone()),
            format!("workspace: {remote}"),
        ),
        (None, false) => {
            let path = chain.cwd.clone();
            (
                RoomId::parse(sanitize_id(&path.to_string_lossy()))
                    .unwrap_or_else(|_| fallback_room()),
                RoomScope::PathPrefix(path.clone()),
                format!("workspace: {}", path.display()),
            )
        }
        (None, true) => (fallback_room(), RoomScope::Global, "global".to_string()),
    };
    Room {
        room_id,
        scope,
        title,
        created_at: now_micros(),
    }
}

fn fallback_room() -> RoomId {
    RoomId::parse("global").expect("`global` is a valid room id")
}

/// Map an arbitrary string to the id alphabet (`[A-Za-z0-9._:-]`), truncated to the id cap.
fn sanitize_id(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.len() > super::ids::MAX_ID_LEN {
        out.truncate(super::ids::MAX_ID_LEN);
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

/// Mint a unique message id from the room, agent, and a microsecond timestamp + a process
/// counter. Collisions are structurally impossible within a single daemon because the counter
/// is monotonic and the daemon is the sole writer.
fn mint_message_id(room: &RoomId, agent: &AgentId) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}:{}:{}:{}",
        room.as_str(),
        agent.as_str(),
        now_micros(),
        n
    )
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
