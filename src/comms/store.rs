//! `CommsStore`: the durable, single-writer Fjall store backing the broker.
//!
//! This is a SECOND, independent Fjall instance (distinct from the per-repo index in
//! `src/index/`), living user-globally under `<data_dir>/comms/`. It mirrors the operational
//! shape of `crate::store::Store`: an exclusive advisory flock (`acquire_lock`) so only one
//! daemon writes, and a `meta`-keyspace schema-version row checked against
//! [`COMMS_SCHEMA_VER`](super::COMMS_SCHEMA_VER) — a mismatch wipes the store and the daemon
//! rebuilds from scratch (comms history is durable-but-disposable scratch, not a source of
//! truth).
//!
//! ## Two-tier message storage
//!
//! [`CommsStore::post`] writes a small [`MessageMeta`] front-matter record to
//! `messages_by_room` AND the body to `message_body`. [`CommsStore::history`] and
//! [`CommsStore::history_with_seq`] decode ONLY the front-matter; the body is fetched lazily
//! via [`CommsStore::get_body`]. The daemon is the sole writer, which is Fjall's happy path.
//!
//! ## Keyspaces
//!
//! `meta`, `rooms`, `messages_by_room`, `message_body`, `subs_by_room`, `cursors`, `agents`,
//! and `sessions`. The `sessions` keyspace maps a terminal `session_id` to a
//! [`SessionLineage`](super::model::SessionLineage) record (parent/child agent + the
//! session-scoped room they share), so a future tree view can reconstruct the spawn graph.
//! Adding it required no [`COMMS_SCHEMA_VER`](super::COMMS_SCHEMA_VER) bump: a brand-new
//! keyspace leaves every existing key/value shape untouched, so an older store still opens and
//! simply has an empty `sessions` partition.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use fs2::FileExt;
use thiserror::Error;

use super::COMMS_SCHEMA_VER;
use super::ids::{AgentId, RoomId};
use super::keys;
use super::model::{
    AgentRecord, MessageBody, MessageMeta, Room, SessionLineage, Subscription, now_micros,
};

const META_SCHEMA_VER: &[u8] = b"schema_ver";
const STORE_DIR: &str = "store.fjall";
const LOCK_FILE: &str = ".lock";

/// Bounded retry while acquiring the advisory flock — mirrors `crate::store::acquire_lock`.
const LOCK_ATTEMPTS: u32 = 25;
const LOCK_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

/// Errors surfaced by the comms store.
#[derive(Debug, Error)]
pub enum CommsStoreError {
    /// A Fjall-level failure.
    #[error("fjall error: {0}")]
    Fjall(#[from] fjall::Error),
    /// An io failure on a concrete path.
    #[error("io error on {path}: {source}")]
    Io {
        /// The path the io operation targeted.
        path: PathBuf,
        /// The underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// msgpack encode failure.
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    /// msgpack decode failure.
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    /// Another daemon already holds the store lock.
    #[error("another basemind comms daemon holds the lock on {0}")]
    Locked(PathBuf),
}

/// Handle to every comms keyspace. Cheap to clone (each `Keyspace` is internally `Arc`'d).
/// The daemon holds one of these and is the sole writer.
pub struct CommsStore {
    db: Database,
    meta: Keyspace,
    rooms: Keyspace,
    messages_by_room: Keyspace,
    message_body: Keyspace,
    subs_by_room: Keyspace,
    cursors: Keyspace,
    agents: Keyspace,
    sessions: Keyspace,
    /// Held for the lifetime of the store; released on drop (Draining → Stopped).
    _lock: File,
}

impl CommsStore {
    /// Open (or create) the comms store under `comms_dir`, taking the exclusive advisory
    /// flock. On a schema-version mismatch the `store.fjall/` directory is wiped and rebuilt
    /// empty.
    pub fn open(comms_dir: &Path) -> Result<Self, CommsStoreError> {
        std::fs::create_dir_all(comms_dir).map_err(|source| CommsStoreError::Io {
            path: comms_dir.to_path_buf(),
            source,
        })?;
        let lock = acquire_lock(comms_dir)?;

        let dir = comms_dir.join(STORE_DIR);
        let needs_wipe = match peek_schema_version(&dir) {
            Some(ver) if ver == COMMS_SCHEMA_VER => false,
            None => false, // brand new
            Some(_) => true,
        };
        if needs_wipe && dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|source| CommsStoreError::Io {
                path: dir.clone(),
                source,
            })?;
        }
        std::fs::create_dir_all(&dir).map_err(|source| CommsStoreError::Io {
            path: dir.clone(),
            source,
        })?;

        let db = Database::builder(&dir).open()?;
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default)?;
        let rooms = db.keyspace("rooms", KeyspaceCreateOptions::default)?;
        let messages_by_room = db.keyspace("messages_by_room", KeyspaceCreateOptions::default)?;
        let message_body = db.keyspace("message_body", KeyspaceCreateOptions::default)?;
        let subs_by_room = db.keyspace("subs_by_room", KeyspaceCreateOptions::default)?;
        let cursors = db.keyspace("cursors", KeyspaceCreateOptions::default)?;
        let agents = db.keyspace("agents", KeyspaceCreateOptions::default)?;
        let sessions = db.keyspace("sessions", KeyspaceCreateOptions::default)?;

        meta.insert(META_SCHEMA_VER, COMMS_SCHEMA_VER.to_be_bytes())?;

        Ok(Self {
            db,
            meta,
            rooms,
            messages_by_room,
            message_body,
            subs_by_room,
            cursors,
            agents,
            sessions,
            _lock: lock,
        })
    }

    // ─── rooms ────────────────────────────────────────────────────────────────────────────

    /// Insert or replace a room record.
    pub fn put_room(&self, room: &Room) -> Result<(), CommsStoreError> {
        let bytes = rmp_serde::to_vec_named(room)?;
        self.rooms
            .insert(keys::room_key(room.room_id.as_str()), bytes)?;
        Ok(())
    }

    /// Fetch a room by id.
    pub fn get_room(&self, room: &RoomId) -> Result<Option<Room>, CommsStoreError> {
        match self.rooms.get(keys::room_key(room.as_str()))? {
            Some(v) => Ok(Some(rmp_serde::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Enumerate every registered room.
    pub fn list_rooms(&self) -> Result<Vec<Room>, CommsStoreError> {
        let mut out = Vec::new();
        for guard in self.rooms.iter() {
            let (_, v) = guard.into_inner()?;
            out.push(rmp_serde::from_slice(&v)?);
        }
        Ok(out)
    }

    // ─── agents ───────────────────────────────────────────────────────────────────────────

    /// Insert or replace an agent record.
    pub fn put_agent(&self, agent: &AgentRecord) -> Result<(), CommsStoreError> {
        let bytes = rmp_serde::to_vec_named(agent)?;
        self.agents
            .insert(keys::agent_key(agent.agent_id.as_str()), bytes)?;
        Ok(())
    }

    /// Fetch an agent record by id.
    pub fn get_agent(&self, agent: &AgentId) -> Result<Option<AgentRecord>, CommsStoreError> {
        match self.agents.get(keys::agent_key(agent.as_str()))? {
            Some(v) => Ok(Some(rmp_serde::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Enumerate every known agent.
    pub fn list_agents(&self) -> Result<Vec<AgentRecord>, CommsStoreError> {
        let mut out = Vec::new();
        for guard in self.agents.iter() {
            let (_, v) = guard.into_inner()?;
            out.push(rmp_serde::from_slice(&v)?);
        }
        Ok(out)
    }

    // ─── sessions (terminal-session lineage) ──────────────────────────────────────────────

    /// Insert or replace a session lineage record, keyed by its `session_id`.
    pub fn put_session(&self, lineage: &SessionLineage) -> Result<(), CommsStoreError> {
        let bytes = rmp_serde::to_vec_named(lineage)?;
        self.sessions
            .insert(keys::session_key(&lineage.session_id), bytes)?;
        Ok(())
    }

    /// Fetch a session lineage record by `session_id`.
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionLineage>, CommsStoreError> {
        match self.sessions.get(keys::session_key(session_id))? {
            Some(v) => Ok(Some(rmp_serde::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Enumerate every recorded session lineage (for a future spawn-tree view).
    pub fn list_sessions(&self) -> Result<Vec<SessionLineage>, CommsStoreError> {
        let mut out = Vec::new();
        for guard in self.sessions.iter() {
            let (_, v) = guard.into_inner()?;
            out.push(rmp_serde::from_slice(&v)?);
        }
        Ok(out)
    }

    /// Remove a session lineage record by `session_id`. Idempotent — removing an
    /// absent id is a no-op. Called when a session is killed so the `sessions`
    /// keyspace does not accumulate dead rows over a long-lived broker.
    pub fn delete_session(&self, session_id: &str) -> Result<(), CommsStoreError> {
        self.sessions.remove(keys::session_key(session_id))?;
        Ok(())
    }

    // ─── subscriptions ────────────────────────────────────────────────────────────────────

    /// Subscribe an agent to a room (idempotent).
    pub fn subscribe(&self, sub: &Subscription) -> Result<(), CommsStoreError> {
        let key = keys::sub_by_room(sub.room.as_str(), sub.agent_id.as_str());
        let bytes = rmp_serde::to_vec_named(sub)?;
        self.subs_by_room.insert(key, bytes)?;
        Ok(())
    }

    /// Unsubscribe an agent from a room.
    pub fn unsubscribe(&self, room: &RoomId, agent: &AgentId) -> Result<(), CommsStoreError> {
        let key = keys::sub_by_room(room.as_str(), agent.as_str());
        self.subs_by_room.remove(key)?;
        Ok(())
    }

    /// List the agents subscribed to a room.
    pub fn subscribers(&self, room: &RoomId) -> Result<Vec<AgentId>, CommsStoreError> {
        let prefix = keys::subs_by_room_prefix(room.as_str());
        let mut out = Vec::new();
        for guard in self.subs_by_room.prefix(prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_, agent)) = keys::parse_sub_by_room(&k)
                && let Ok(id) = AgentId::parse(agent)
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Every room an agent is subscribed to. A full scan of `subs_by_room` — acceptable for
    /// the inbox path because subscription counts are small (rooms per agent, not messages).
    pub fn rooms_for_agent(&self, agent: &AgentId) -> Result<Vec<RoomId>, CommsStoreError> {
        let mut out = Vec::new();
        for guard in self.subs_by_room.iter() {
            let (k, _) = guard.into_inner()?;
            if let Some((room, a)) = keys::parse_sub_by_room(&k)
                && a == agent.as_str()
                && let Ok(id) = RoomId::parse(room)
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    // ─── messages ─────────────────────────────────────────────────────────────────────────

    /// Read the current `seq` counter for a room (0 if unset). Single-writer, so the
    /// read-modify-write in [`post`](Self::post) needs no CAS; the bumped value is staged into
    /// the same batch as the message so a crash can never consume a `seq` without storing a
    /// message at it.
    fn current_seq(&self, room: &RoomId) -> Result<u64, CommsStoreError> {
        let key = keys::room_seq_meta_key(room.as_str());
        Ok(match self.meta.get(&key)? {
            Some(v) if v.len() == 8 => {
                u64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]])
            }
            _ => 0,
        })
    }

    /// Store a message: front-matter to `messages_by_room`, body to `message_body`. Returns
    /// the persisted [`MessageMeta`] (with its allocated `seq`-bearing key already written).
    /// The two writes plus the seq-counter bump go through one atomic batch, so the counter
    /// never advances without a corresponding message landing.
    pub fn post(
        &self,
        room: &RoomId,
        meta: MessageMeta,
        body: MessageBody,
    ) -> Result<(u64, MessageMeta), CommsStoreError> {
        let seq = self.current_seq(room)?.saturating_add(1);
        let mut batch = self.db.batch();
        batch.insert(
            &self.meta,
            keys::room_seq_meta_key(room.as_str()),
            seq.to_be_bytes(),
        );
        let meta_key = keys::message_by_room(room.as_str(), seq);
        let meta_bytes = rmp_serde::to_vec_named(&meta)?;
        batch.insert(&self.messages_by_room, meta_key, meta_bytes);
        let body_bytes = rmp_serde::to_vec_named(&body)?;
        batch.insert(&self.message_body, meta.id.as_bytes().to_vec(), body_bytes);
        batch.commit()?;
        Ok((seq, meta))
    }

    /// Read a room's history starting AFTER `after_seq` (exclusive), oldest-first, up to
    /// `limit`. Decodes ONLY [`MessageMeta`] — never the body. Returns the records plus the
    /// last `seq` seen (for the next cursor) and whether more remain.
    pub fn history(
        &self,
        room: &RoomId,
        after_seq: u64,
        limit: usize,
    ) -> Result<HistoryPage, CommsStoreError> {
        let prefix = keys::messages_by_room_prefix(room.as_str());
        let mut messages = Vec::new();
        let mut last_seq = after_seq;
        let mut more = false;
        for guard in self.messages_by_room.prefix(&prefix) {
            let (k, v) = guard.into_inner()?;
            let Some((_, seq)) = keys::parse_message_by_room(&k) else {
                continue;
            };
            if seq <= after_seq {
                continue;
            }
            if messages.len() >= limit {
                more = true;
                break;
            }
            let meta: MessageMeta = rmp_serde::from_slice(&v)?;
            messages.push((seq, meta));
            last_seq = seq;
        }
        Ok(HistoryPage {
            messages,
            last_seq,
            more,
        })
    }

    /// Like [`CommsStore::history`] but yields `(seq, MessageMeta)` pairs. The inbox path uses
    /// the seqs to advance per-room read cursors. Front-matter only — never the body.
    pub fn history_with_seq(
        &self,
        room: &RoomId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<(u64, MessageMeta)>, CommsStoreError> {
        let prefix = keys::messages_by_room_prefix(room.as_str());
        let mut out = Vec::new();
        for guard in self.messages_by_room.prefix(&prefix) {
            let (k, v) = guard.into_inner()?;
            let Some((_, seq)) = keys::parse_message_by_room(&k) else {
                continue;
            };
            if seq <= after_seq {
                continue;
            }
            if out.len() >= limit {
                break;
            }
            out.push((seq, rmp_serde::from_slice(&v)?));
        }
        Ok(out)
    }

    /// Fetch a message body by id from `message_body`. The ONLY path that touches a body.
    pub fn get_body(&self, message_id: &str) -> Result<Option<Vec<u8>>, CommsStoreError> {
        match self.message_body.get(message_id.as_bytes())? {
            Some(v) => {
                let body: MessageBody = rmp_serde::from_slice(&v)?;
                Ok(Some(body.0))
            }
            None => Ok(None),
        }
    }

    /// Resolve a batch of message ids to their `(room, seq)` positions in a SINGLE scan of the
    /// front-matter index. Returns one entry per id that was found (unknown ids are skipped).
    ///
    /// There is no dedicated `id → (room, seq)` reverse index, so this walks `messages_by_room`
    /// (front-matter only — never a body), bounded by the front-matter count not body sizes. The
    /// single-pass batch design keeps `inbox_ack` over many ids at one `messages_by_room` walk
    /// rather than one walk per id. A reverse index is the future optimization if this scan ever
    /// becomes hot (tracked alongside the comms schema version).
    pub fn resolve_ids(
        &self,
        message_ids: &[String],
    ) -> Result<Vec<(String, RoomId, u64)>, CommsStoreError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: ahash::AHashSet<&str> = message_ids.iter().map(String::as_str).collect();
        let mut out = Vec::with_capacity(message_ids.len());
        for guard in self.messages_by_room.iter() {
            let (k, v) = guard.into_inner()?;
            let Some((_, seq)) = keys::parse_message_by_room(&k) else {
                continue;
            };
            let meta: MessageMeta = rmp_serde::from_slice(&v)?;
            if wanted.contains(meta.id.as_str()) {
                out.push((meta.id.clone(), meta.room, seq));
                if out.len() == wanted.len() {
                    break;
                }
            }
        }
        Ok(out)
    }

    // ─── read cursors (per agent, per room) ───────────────────────────────────────────────

    /// The agent's last-read `seq` for a room (0 when never read).
    pub fn read_cursor(&self, agent: &AgentId, room: &RoomId) -> Result<u64, CommsStoreError> {
        let key = keys::cursor_key(agent.as_str(), room.as_str());
        match self.cursors.get(key)? {
            Some(v) if v.len() == 8 => Ok(u64::from_be_bytes([
                v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7],
            ])),
            _ => Ok(0),
        }
    }

    /// Advance the agent's read cursor for a room to `seq` (monotonic; never moves backward).
    pub fn set_read_cursor(
        &self,
        agent: &AgentId,
        room: &RoomId,
        seq: u64,
    ) -> Result<(), CommsStoreError> {
        let current = self.read_cursor(agent, room)?;
        if seq <= current {
            return Ok(());
        }
        let key = keys::cursor_key(agent.as_str(), room.as_str());
        self.cursors.insert(key, seq.to_be_bytes())?;
        Ok(())
    }
}

/// One page of history: the decoded front-matter, the last `seq` seen, and whether the scan
/// stopped early because `limit` was hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    /// The front-matter records in this page, oldest-first, each paired with its per-room `seq`.
    pub messages: Vec<(u64, MessageMeta)>,
    /// The `seq` of the last record returned (or the input `after_seq` when empty).
    pub last_seq: u64,
    /// True when more records remain after this page.
    pub more: bool,
}

/// Compute the content hash (hex) of a body — surfaced as [`MessageMeta::body_sha`]. Uses the
/// project's blake3 hasher (the `_sha` name is generic "content hash", not literally SHA-2).
pub fn body_hash_hex(body: &[u8]) -> String {
    crate::hashing::hex(&crate::hashing::hash_bytes(body))
}

/// Build the front-matter for a post. The id is the body hash + timestamp + sequence-free
/// uniqueness via the room and microsecond timestamp; callers should ensure uniqueness by
/// passing a unique `id` (the daemon uses `room:ts:agent`-derived ids).
#[allow(clippy::too_many_arguments)]
pub fn build_meta(
    id: String,
    room: RoomId,
    from: AgentId,
    subject: String,
    tags: Vec<String>,
    reply_to: Option<String>,
    scope: Vec<String>,
    body: &[u8],
) -> MessageMeta {
    MessageMeta {
        id,
        room,
        from,
        ts_micros: now_micros(),
        subject,
        tags,
        reply_to,
        scope,
        body_len: u32::try_from(body.len()).unwrap_or(u32::MAX),
        body_sha: body_hash_hex(body),
    }
}

/// Acquire the comms store's advisory `.lock` (exclusive flock, bounded retry). Mirrors
/// `crate::store::acquire_lock`.
fn acquire_lock(comms_dir: &Path) -> Result<File, CommsStoreError> {
    let path = comms_dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| CommsStoreError::Io {
            path: path.clone(),
            source,
        })?;
    for attempt in 0..LOCK_ATTEMPTS {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(_) if attempt + 1 < LOCK_ATTEMPTS => std::thread::sleep(LOCK_BACKOFF),
            Err(_) => return Err(CommsStoreError::Locked(path)),
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// Best-effort peek at the on-disk schema version. `None` when the store dir is absent or the
/// meta row is unreadable. Mirrors `crate::index::peek_schema_version`.
fn peek_schema_version(dir: &Path) -> Option<u32> {
    if !dir.exists() {
        return None;
    }
    let db = Database::builder(dir).open().ok()?;
    let meta = db.keyspace("meta", KeyspaceCreateOptions::default).ok()?;
    let bytes = meta.get(META_SCHEMA_VER).ok().flatten()?;
    if bytes.len() != 4 {
        return None;
    }
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comms::model::AgentCard;

    fn temp_store() -> (tempfile::TempDir, CommsStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommsStore::open(dir.path()).expect("open store");
        (dir, store)
    }

    fn room_id(s: &str) -> RoomId {
        RoomId::parse(s).expect("room")
    }

    fn agent_id(s: &str) -> AgentId {
        AgentId::parse(s).expect("agent")
    }

    #[test]
    fn post_then_history_returns_meta_and_body_is_not_loaded() {
        let (_d, store) = temp_store();
        let room = room_id("room-1");
        store
            .put_room(&Room {
                room_id: room.clone(),
                scope: super::super::model::RoomScope::Global,
                title: "t".to_string(),
                created_at: now_micros(),
            })
            .expect("put room");

        let body = b"the quick brown fox".to_vec();
        let meta = build_meta(
            "m-1".to_string(),
            room.clone(),
            agent_id("agent-1"),
            "subj".to_string(),
            vec![],
            None,
            vec![],
            &body,
        );
        let (seq, _) = store
            .post(&room, meta.clone(), MessageBody(body.clone()))
            .expect("post");
        assert_eq!(seq, 1, "first message in a room gets seq 1");

        let page = store.history(&room, 0, 10).expect("history");
        assert_eq!(page.messages.len(), 1);
        let (got_seq, got) = &page.messages[0];
        assert_eq!(
            *got_seq, 1,
            "history pairs each record with its per-room seq"
        );
        // History returns the front-matter, including the body length + hash, but NOT the
        // body itself — `MessageMeta` has no body field.
        assert_eq!(got.id, "m-1");
        assert_eq!(got.subject, "subj");
        assert_eq!(got.body_len as usize, body.len());
        assert_eq!(got.body_sha, body_hash_hex(&body));

        // The body is fetched only on demand, from the separate keyspace.
        let fetched = store.get_body("m-1").expect("get_body");
        assert_eq!(fetched.as_deref(), Some(body.as_slice()));
        assert_eq!(store.get_body("nope").expect("get_body"), None);
    }

    #[test]
    fn history_paginates_by_seq() {
        let (_d, store) = temp_store();
        let room = room_id("room-1");
        for i in 0..5u32 {
            let body = format!("body-{i}").into_bytes();
            let meta = build_meta(
                format!("m-{i}"),
                room.clone(),
                agent_id("a"),
                format!("s-{i}"),
                vec![],
                None,
                vec![],
                &body,
            );
            store.post(&room, meta, MessageBody(body)).expect("post");
        }
        let page1 = store.history(&room, 0, 2).expect("history");
        assert_eq!(page1.messages.len(), 2);
        assert!(page1.more);
        let page2 = store.history(&room, page1.last_seq, 2).expect("history");
        assert_eq!(page2.messages.len(), 2);
        assert_eq!(page2.messages[0].1.id, "m-2");
    }

    /// The seq counter is bumped inside the same atomic batch as the message, so it persists
    /// with the message and a reopened store keeps allocating strictly increasing seqs — no
    /// reuse of an existing seq (which would overwrite a message) and no off-by-one reset.
    #[test]
    fn seq_counter_persists_across_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let room = room_id("room-1");
        let post = |store: &CommsStore, id: &str| {
            let body = id.as_bytes().to_vec();
            let meta = build_meta(
                id.to_string(),
                room.clone(),
                agent_id("a"),
                id.to_string(),
                vec![],
                None,
                vec![],
                &body,
            );
            store.post(&room, meta, MessageBody(body)).expect("post").0
        };
        {
            let store = CommsStore::open(dir.path()).expect("open");
            assert_eq!(post(&store, "m-1"), 1);
            assert_eq!(post(&store, "m-2"), 2);
        }
        // Reopen: the next seq must continue from the persisted counter, not restart at 1.
        {
            let store = CommsStore::open(dir.path()).expect("reopen");
            assert_eq!(post(&store, "m-3"), 3, "seq must continue past reopen");
            // All three messages survive with no overwrite.
            let page = store.history(&room, 0, 10).expect("history");
            assert_eq!(page.messages.len(), 3, "no message lost or overwritten");
            let ids: Vec<&str> = page.messages.iter().map(|(_, m)| m.id.as_str()).collect();
            assert_eq!(ids, ["m-1", "m-2", "m-3"]);
        }
    }

    #[test]
    fn subscriptions_round_trip() {
        let (_d, store) = temp_store();
        let room = room_id("room-1");
        let agent = agent_id("agent-1");
        store
            .subscribe(&Subscription {
                agent_id: agent.clone(),
                room: room.clone(),
                created_at: now_micros(),
            })
            .expect("subscribe");
        assert_eq!(store.subscribers(&room).expect("subs"), vec![agent.clone()]);
        assert_eq!(
            store.rooms_for_agent(&agent).expect("rooms"),
            vec![room.clone()]
        );
        store.unsubscribe(&room, &agent).expect("unsub");
        assert!(store.subscribers(&room).expect("subs").is_empty());
    }

    #[test]
    fn read_cursor_is_monotonic() {
        let (_d, store) = temp_store();
        let room = room_id("room-1");
        let agent = agent_id("agent-1");
        assert_eq!(store.read_cursor(&agent, &room).expect("read"), 0);
        store.set_read_cursor(&agent, &room, 5).expect("set");
        assert_eq!(store.read_cursor(&agent, &room).expect("read"), 5);
        // Moving backward is a no-op.
        store.set_read_cursor(&agent, &room, 3).expect("set");
        assert_eq!(store.read_cursor(&agent, &room).expect("read"), 5);
    }

    #[test]
    fn resolve_ids_maps_each_id_to_its_room_and_seq() {
        let (_d, store) = temp_store();
        let room_a = room_id("room-a");
        let room_b = room_id("room-b");
        let mk = |store: &CommsStore, room: &RoomId, id: &str| {
            let body = id.as_bytes().to_vec();
            let meta = build_meta(
                id.to_string(),
                room.clone(),
                agent_id("a"),
                id.to_string(),
                vec![],
                None,
                vec![],
                &body,
            );
            store.post(room, meta, MessageBody(body)).expect("post").0
        };
        let s_a1 = mk(&store, &room_a, "m-a1");
        let _s_a2 = mk(&store, &room_a, "m-a2");
        let s_b1 = mk(&store, &room_b, "m-b1");

        // Batch resolver groups across rooms; unknown ids are dropped.
        let mut got = store
            .resolve_ids(&["m-a1".to_string(), "m-b1".to_string(), "ghost".to_string()])
            .expect("resolve_ids");
        got.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(
            got,
            vec![
                ("m-a1".to_string(), room_a.clone(), s_a1),
                ("m-b1".to_string(), room_b.clone(), s_b1),
            ]
        );
        // Empty input short-circuits to an empty result with no scan.
        assert!(store.resolve_ids(&[]).expect("resolve_ids").is_empty());
    }

    #[test]
    fn session_lineage_round_trips_and_lists() {
        use crate::comms::model::SessionLineage;
        let (_d, store) = temp_store();
        let lineage = SessionLineage {
            session_id: "sess-abc".to_string(),
            parent_agent: Some(agent_id("parent")),
            child_agent: agent_id("child"),
            room_id: room_id("session-sess-abc"),
            created_at: now_micros(),
        };
        assert_eq!(store.get_session("sess-abc").expect("get"), None);
        store.put_session(&lineage).expect("put");
        assert_eq!(
            store.get_session("sess-abc").expect("get"),
            Some(lineage.clone())
        );

        // A second, parentless session lists alongside the first.
        let orphan = SessionLineage {
            session_id: "sess-def".to_string(),
            parent_agent: None,
            child_agent: agent_id("solo"),
            room_id: room_id("session-sess-def"),
            created_at: now_micros(),
        };
        store.put_session(&orphan).expect("put");
        let mut all = store.list_sessions().expect("list");
        all.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        assert_eq!(all, vec![lineage, orphan]);
    }

    #[test]
    fn agent_records_round_trip() {
        let (_d, store) = temp_store();
        let rec = AgentRecord {
            agent_id: agent_id("agent-1"),
            card: AgentCard {
                name: "n".to_string(),
                description: "d".to_string(),
                version: "1".to_string(),
                skills: vec![],
            },
            kind: super::super::model::AgentKind::Cli,
            first_seen: now_micros(),
            last_seen: now_micros(),
        };
        store.put_agent(&rec).expect("put");
        assert_eq!(
            store.get_agent(&agent_id("agent-1")).expect("get"),
            Some(rec)
        );
    }
}
