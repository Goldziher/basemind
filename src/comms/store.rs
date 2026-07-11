//! `CommsStore`: the durable, single-writer Fjall store backing the broker.
//!
//! This is a SECOND, independent Fjall instance (distinct from the per-repo index in
//! `src/index/`), living user-globally under `<data_dir>/comms/`. It mirrors the operational
//! shape of `crate::store::Store`: an exclusive advisory flock (`acquire_lock`) so only one
//! daemon writes, and a `meta`-keyspace schema-version row checked against
//! `COMMS_SCHEMA_VER` — a mismatch wipes the store and the daemon
//! rebuilds from scratch (comms history is durable-but-disposable scratch, not a source of
//! truth).
//!
//! ## Two-tier message storage
//!
//! [`CommsStore::post`] writes a small [`MessageMeta`] front-matter record to
//! `messages_by_thread` AND the body to `message_body`. [`CommsStore::history`] and
//! [`CommsStore::history_with_seq`] decode ONLY the front-matter; the body is fetched lazily
//! via [`CommsStore::get_body`]. The daemon is the sole writer, which is Fjall's happy path.
//!
//! ## Keyspaces
//!
//! `meta`, `threads`, `thread_members`, `messages_by_thread`, `message_body`, `thread_subs`,
//! `cursors`, and `agents`. `thread_members` is the durable member set (drives inbox + creator
//! authorization); `thread_subs` mirrors it for the notification-stream fan-out.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use fs2::FileExt;
use thiserror::Error;

use super::COMMS_SCHEMA_VER;
use super::ids::{AgentId, ThreadId};
use super::keys;
use super::model::{AgentRecord, Membership, MessageBody, MessageMeta, Thread, now_micros};

const META_SCHEMA_VER: &[u8] = b"schema_ver";
const STORE_DIR: &str = "store.fjall";
const LOCK_FILE: &str = ".lock";

/// Default retention for thread messages. The daemon's periodic prune sweep deletes any message
/// (front-matter + body) whose `ts_micros` is older than this, so coordination history cannot
/// grow without bound.
pub const MESSAGE_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

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
    threads: Keyspace,
    thread_members: Keyspace,
    messages_by_thread: Keyspace,
    message_body: Keyspace,
    thread_subs: Keyspace,
    cursors: Keyspace,
    agents: Keyspace,
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
        std::fs::create_dir_all(&dir).map_err(|source| CommsStoreError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut db = Database::builder(&dir).open()?;
        let mut meta = db.keyspace("meta", KeyspaceCreateOptions::default)?;
        let on_disk_ver = meta
            .get(META_SCHEMA_VER)?
            .and_then(|bytes| <[u8; 4]>::try_from(&bytes[..]).ok())
            .map(u32::from_be_bytes);
        if matches!(on_disk_ver, Some(ver) if ver != COMMS_SCHEMA_VER) {
            drop(meta);
            drop(db);
            std::fs::remove_dir_all(&dir).map_err(|source| CommsStoreError::Io {
                path: dir.clone(),
                source,
            })?;
            std::fs::create_dir_all(&dir).map_err(|source| CommsStoreError::Io {
                path: dir.clone(),
                source,
            })?;
            db = Database::builder(&dir).open()?;
            meta = db.keyspace("meta", KeyspaceCreateOptions::default)?;
        }
        let threads = db.keyspace("threads", KeyspaceCreateOptions::default)?;
        let thread_members = db.keyspace("thread_members", KeyspaceCreateOptions::default)?;
        let messages_by_thread = db.keyspace("messages_by_thread", KeyspaceCreateOptions::default)?;
        let message_body = db.keyspace("message_body", KeyspaceCreateOptions::default)?;
        let thread_subs = db.keyspace("thread_subs", KeyspaceCreateOptions::default)?;
        let cursors = db.keyspace("cursors", KeyspaceCreateOptions::default)?;
        let agents = db.keyspace("agents", KeyspaceCreateOptions::default)?;

        meta.insert(META_SCHEMA_VER, COMMS_SCHEMA_VER.to_be_bytes())?;

        Ok(Self {
            db,
            meta,
            threads,
            thread_members,
            messages_by_thread,
            message_body,
            thread_subs,
            cursors,
            agents,
            _lock: lock,
        })
    }

    /// Insert or replace a thread record.
    pub fn put_thread(&self, thread: &Thread) -> Result<(), CommsStoreError> {
        let bytes = rmp_serde::to_vec_named(thread)?;
        self.threads.insert(keys::thread_key(thread.id.as_str()), bytes)?;
        Ok(())
    }

    /// Fetch a thread by id.
    pub fn get_thread(&self, thread: &ThreadId) -> Result<Option<Thread>, CommsStoreError> {
        match self.threads.get(keys::thread_key(thread.as_str()))? {
            Some(v) => Ok(Some(rmp_serde::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Enumerate every registered thread (active and archived).
    pub fn list_threads(&self) -> Result<Vec<Thread>, CommsStoreError> {
        let mut out = Vec::new();
        for guard in self.threads.iter() {
            let (_, v) = guard.into_inner()?;
            out.push(rmp_serde::from_slice(&v)?);
        }
        Ok(out)
    }

    /// Insert or replace an agent record.
    pub fn put_agent(&self, agent: &AgentRecord) -> Result<(), CommsStoreError> {
        let bytes = rmp_serde::to_vec_named(agent)?;
        self.agents.insert(keys::agent_key(agent.agent_id.as_str()), bytes)?;
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

    /// Add an agent to a thread's membership (idempotent). Writes the durable `thread_members`
    /// row AND the `thread_subs` mirror the notification fan-out reads.
    pub fn add_member(&self, membership: &Membership) -> Result<(), CommsStoreError> {
        let key = keys::thread_agent(membership.thread.as_str(), membership.agent_id.as_str());
        let bytes = rmp_serde::to_vec_named(membership)?;
        self.thread_members.insert(&key, &bytes)?;
        self.thread_subs.insert(&key, &bytes)?;
        Ok(())
    }

    /// Remove an agent from a thread's membership.
    pub fn remove_member(&self, thread: &ThreadId, agent: &AgentId) -> Result<(), CommsStoreError> {
        let key = keys::thread_agent(thread.as_str(), agent.as_str());
        self.thread_members.remove(&key)?;
        self.thread_subs.remove(&key)?;
        Ok(())
    }

    /// List the agents that are members of a thread.
    pub fn members(&self, thread: &ThreadId) -> Result<Vec<AgentId>, CommsStoreError> {
        let prefix = keys::thread_agent_prefix(thread.as_str());
        let mut out = Vec::new();
        for guard in self.thread_members.prefix(prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_, agent)) = keys::parse_thread_agent(&k)
                && let Ok(id) = AgentId::parse(agent)
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// True when `agent` is a member of `thread`.
    pub fn is_member(&self, thread: &ThreadId, agent: &AgentId) -> Result<bool, CommsStoreError> {
        let key = keys::thread_agent(thread.as_str(), agent.as_str());
        Ok(self.thread_members.get(key)?.is_some())
    }

    /// Every thread an agent is a member of. A full scan of `thread_members` — acceptable for
    /// the inbox path because membership counts are small (threads per agent, not messages).
    pub fn threads_for_agent(&self, agent: &AgentId) -> Result<Vec<ThreadId>, CommsStoreError> {
        let mut out = Vec::new();
        for guard in self.thread_members.iter() {
            let (k, _) = guard.into_inner()?;
            if let Some((thread, a)) = keys::parse_thread_agent(&k)
                && a == agent.as_str()
                && let Ok(id) = ThreadId::parse(thread)
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Read the current `seq` counter for a thread (0 if unset). Single-writer, so the
    /// read-modify-write in [`post`](Self::post) needs no CAS; the bumped value is staged into
    /// the same batch as the message so a crash can never consume a `seq` without storing a
    /// message at it.
    fn current_seq(&self, thread: &ThreadId) -> Result<u64, CommsStoreError> {
        let key = keys::thread_seq_meta_key(thread.as_str());
        Ok(match self.meta.get(&key)? {
            Some(v) if v.len() == 8 => u64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]]),
            _ => 0,
        })
    }

    /// Store a message: front-matter to `messages_by_thread`, body to `message_body`. Returns
    /// the allocated `seq` and the persisted [`MessageMeta`]. The two writes plus the seq-counter
    /// bump go through one atomic batch, so the counter never advances without a corresponding
    /// message landing.
    pub fn post(
        &self,
        thread: &ThreadId,
        meta: MessageMeta,
        body: MessageBody,
    ) -> Result<(u64, MessageMeta), CommsStoreError> {
        let seq = self.current_seq(thread)?.saturating_add(1);
        let mut batch = self.db.batch();
        batch.insert(
            &self.meta,
            keys::thread_seq_meta_key(thread.as_str()),
            seq.to_be_bytes(),
        );
        let meta_key = keys::message_by_thread(thread.as_str(), seq);
        let meta_bytes = rmp_serde::to_vec_named(&meta)?;
        batch.insert(&self.messages_by_thread, meta_key, meta_bytes);
        let body_bytes = rmp_serde::to_vec_named(&body)?;
        batch.insert(&self.message_body, meta.id.as_bytes().to_vec(), body_bytes);
        batch.commit()?;
        Ok((seq, meta))
    }

    /// Read a thread's history starting AFTER `after_seq` (exclusive), oldest-first, up to
    /// `limit`. Decodes ONLY [`MessageMeta`] — never the body.
    pub fn history(&self, thread: &ThreadId, after_seq: u64, limit: usize) -> Result<HistoryPage, CommsStoreError> {
        let prefix = keys::messages_by_thread_prefix(thread.as_str());
        let mut messages = Vec::new();
        let mut last_seq = after_seq;
        let mut more = false;
        for guard in self.messages_by_thread.prefix(&prefix) {
            let (k, v) = guard.into_inner()?;
            let Some((_, seq)) = keys::parse_message_by_thread(&k) else {
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
    /// the seqs to advance per-thread read cursors. Front-matter only — never the body.
    pub fn history_with_seq(
        &self,
        thread: &ThreadId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<(u64, MessageMeta)>, CommsStoreError> {
        let prefix = keys::messages_by_thread_prefix(thread.as_str());
        let mut out = Vec::new();
        for guard in self.messages_by_thread.prefix(&prefix) {
            let (k, v) = guard.into_inner()?;
            let Some((_, seq)) = keys::parse_message_by_thread(&k) else {
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

    /// Delete every message whose `ts_micros` is older than `now - ttl`, removing both the
    /// front-matter (`messages_by_thread`) and the body (`message_body`) in one atomic batch.
    /// Returns the number of messages pruned.
    pub fn prune_expired(&self, ttl: std::time::Duration) -> Result<usize, CommsStoreError> {
        let ttl_micros = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
        let cutoff = now_micros().saturating_sub(ttl_micros);
        let mut batch = self.db.batch();
        let mut pruned = 0usize;
        for guard in self.messages_by_thread.iter() {
            let (k, v) = guard.into_inner()?;
            let meta: MessageMeta = rmp_serde::from_slice(&v)?;
            if meta.ts_micros < cutoff {
                batch.remove(&self.messages_by_thread, k.to_vec());
                batch.remove(&self.message_body, meta.id.as_bytes().to_vec());
                pruned += 1;
            }
        }
        if pruned > 0 {
            batch.commit()?;
        }
        Ok(pruned)
    }

    /// Archive every ACTIVE thread whose `last_activity` (or `created_at`, when it has never had a
    /// post) is older than `now - ttl`. Returns the number of threads archived. The system's
    /// idle-thread auto-archive — the sanctioned migration for a stale thread. Archived threads and
    /// their history remain readable; they simply drop out of active listings.
    pub fn archive_idle(&self, ttl: std::time::Duration) -> Result<usize, CommsStoreError> {
        let ttl_micros = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
        let cutoff = now_micros().saturating_sub(ttl_micros);
        let mut archived = 0usize;
        for thread in self.list_threads()? {
            if !thread.active {
                continue;
            }
            let last = if thread.last_activity > 0 {
                thread.last_activity
            } else {
                thread.created_at
            };
            if last < cutoff {
                let mut updated = thread;
                updated.active = false;
                self.put_thread(&updated)?;
                archived += 1;
            }
        }
        Ok(archived)
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

    /// Resolve a batch of message ids to their `(thread, seq)` positions in a SINGLE scan of the
    /// front-matter index. Returns one entry per id that was found (unknown ids are skipped).
    pub fn resolve_ids(&self, message_ids: &[String]) -> Result<Vec<(String, ThreadId, u64)>, CommsStoreError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: ahash::AHashSet<&str> = message_ids.iter().map(String::as_str).collect();
        let mut out = Vec::with_capacity(message_ids.len());
        for guard in self.messages_by_thread.iter() {
            let (k, v) = guard.into_inner()?;
            let Some((_, seq)) = keys::parse_message_by_thread(&k) else {
                continue;
            };
            let meta: MessageMeta = rmp_serde::from_slice(&v)?;
            if wanted.contains(meta.id.as_str()) {
                out.push((meta.id.clone(), meta.thread, seq));
                if out.len() == wanted.len() {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The agent's last-read `seq` for a thread (0 when never read).
    pub fn read_cursor(&self, agent: &AgentId, thread: &ThreadId) -> Result<u64, CommsStoreError> {
        let key = keys::cursor_key(agent.as_str(), thread.as_str());
        match self.cursors.get(key)? {
            Some(v) if v.len() == 8 => Ok(u64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]])),
            _ => Ok(0),
        }
    }

    /// Advance the agent's read cursor for a thread to `seq` (monotonic; never moves backward).
    pub fn set_read_cursor(&self, agent: &AgentId, thread: &ThreadId, seq: u64) -> Result<(), CommsStoreError> {
        let current = self.read_cursor(agent, thread)?;
        if seq <= current {
            return Ok(());
        }
        let key = keys::cursor_key(agent.as_str(), thread.as_str());
        self.cursors.insert(key, seq.to_be_bytes())?;
        Ok(())
    }
}

/// One page of history: the decoded front-matter, the last `seq` seen, and whether the scan
/// stopped early because `limit` was hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    /// The front-matter records in this page, oldest-first, each paired with its per-thread `seq`.
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

/// Build the front-matter for a post. Callers should ensure uniqueness by passing a unique `id`
/// (the daemon uses `thread:ts:agent`-derived ids).
pub fn build_meta(
    id: String,
    thread: ThreadId,
    from: AgentId,
    subject: String,
    tags: Vec<String>,
    reply_to: Option<String>,
    body: &[u8],
) -> MessageMeta {
    MessageMeta {
        id,
        thread,
        from,
        ts_micros: now_micros(),
        subject,
        tags,
        reply_to,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comms::model::AgentCard;

    fn temp_store() -> (tempfile::TempDir, CommsStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CommsStore::open(dir.path()).expect("open store");
        (dir, store)
    }

    fn thread_id(s: &str) -> ThreadId {
        ThreadId::parse(s).expect("thread")
    }

    fn agent_id(s: &str) -> AgentId {
        AgentId::parse(s).expect("agent")
    }

    fn sample_thread(id: &str) -> Thread {
        Thread {
            id: thread_id(id),
            subject: Some("topic".to_string()),
            path: None,
            members: vec![agent_id("a")],
            creator: agent_id("a"),
            active: true,
            created_at: now_micros(),
            last_activity: 0,
        }
    }

    #[test]
    fn post_then_history_returns_meta_and_body_is_not_loaded() {
        let (_d, store) = temp_store();
        let thread = thread_id("th-1");
        store.put_thread(&sample_thread("th-1")).expect("put thread");

        let body = b"the quick brown fox".to_vec();
        let meta = build_meta(
            "m-1".to_string(),
            thread.clone(),
            agent_id("agent-1"),
            "subj".to_string(),
            vec![],
            None,
            &body,
        );
        let (seq, _) = store
            .post(&thread, meta.clone(), MessageBody(body.clone()))
            .expect("post");
        assert_eq!(seq, 1, "first message in a thread gets seq 1");

        let page = store.history(&thread, 0, 10).expect("history");
        assert_eq!(page.messages.len(), 1);
        let (got_seq, got) = &page.messages[0];
        assert_eq!(*got_seq, 1);
        assert_eq!(got.id, "m-1");
        assert_eq!(got.subject, "subj");
        assert_eq!(got.body_len as usize, body.len());
        assert_eq!(got.body_sha, body_hash_hex(&body));

        let fetched = store.get_body("m-1").expect("get_body");
        assert_eq!(fetched.as_deref(), Some(body.as_slice()));
        assert_eq!(store.get_body("nope").expect("get_body"), None);
    }

    #[test]
    fn history_paginates_by_seq() {
        let (_d, store) = temp_store();
        let thread = thread_id("th-1");
        for i in 0..5u32 {
            let body = format!("body-{i}").into_bytes();
            let meta = build_meta(
                format!("m-{i}"),
                thread.clone(),
                agent_id("a"),
                format!("s-{i}"),
                vec![],
                None,
                &body,
            );
            store.post(&thread, meta, MessageBody(body)).expect("post");
        }
        let page1 = store.history(&thread, 0, 2).expect("history");
        assert_eq!(page1.messages.len(), 2);
        assert!(page1.more);
        let page2 = store.history(&thread, page1.last_seq, 2).expect("history");
        assert_eq!(page2.messages.len(), 2);
        assert_eq!(page2.messages[0].1.id, "m-2");
    }

    #[test]
    fn seq_counter_persists_across_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let thread = thread_id("th-1");
        let post = |store: &CommsStore, id: &str| {
            let body = id.as_bytes().to_vec();
            let meta = build_meta(
                id.to_string(),
                thread.clone(),
                agent_id("a"),
                id.to_string(),
                vec![],
                None,
                &body,
            );
            store.post(&thread, meta, MessageBody(body)).expect("post").0
        };
        {
            let store = CommsStore::open(dir.path()).expect("open");
            assert_eq!(post(&store, "m-1"), 1);
            assert_eq!(post(&store, "m-2"), 2);
        }
        {
            let store = CommsStore::open(dir.path()).expect("reopen");
            assert_eq!(post(&store, "m-3"), 3, "seq must continue past reopen");
            let page = store.history(&thread, 0, 10).expect("history");
            assert_eq!(page.messages.len(), 3);
            let ids: Vec<&str> = page.messages.iter().map(|(_, m)| m.id.as_str()).collect();
            assert_eq!(ids, ["m-1", "m-2", "m-3"]);
        }
    }

    #[test]
    fn prune_expired_deletes_old_messages_and_bodies_but_keeps_recent() {
        let (_d, store) = temp_store();
        let thread = thread_id("th-1");
        let stale_body = b"stale".to_vec();
        let mut stale = build_meta(
            "old".to_string(),
            thread.clone(),
            agent_id("a"),
            "old".to_string(),
            vec![],
            None,
            &stale_body,
        );
        stale.ts_micros = now_micros() - 10 * 24 * 60 * 60 * 1_000_000;
        store.post(&thread, stale, MessageBody(stale_body)).expect("post stale");
        let fresh_body = b"fresh".to_vec();
        let fresh = build_meta(
            "new".to_string(),
            thread.clone(),
            agent_id("a"),
            "new".to_string(),
            vec![],
            None,
            &fresh_body,
        );
        store.post(&thread, fresh, MessageBody(fresh_body)).expect("post fresh");

        let pruned = store
            .prune_expired(std::time::Duration::from_secs(24 * 60 * 60))
            .expect("prune");
        assert_eq!(pruned, 1, "exactly the stale message is pruned");

        let page = store.history(&thread, 0, 10).expect("history");
        let ids: Vec<&str> = page.messages.iter().map(|(_, m)| m.id.as_str()).collect();
        assert_eq!(ids, ["new"]);
        assert_eq!(store.get_body("old").expect("get_body"), None);
    }

    #[test]
    fn archive_idle_flips_only_stale_active_threads() {
        let (_d, store) = temp_store();
        let mut stale = sample_thread("stale");
        stale.last_activity = now_micros() - 30 * 24 * 60 * 60 * 1_000_000;
        store.put_thread(&stale).expect("put stale");
        let mut fresh = sample_thread("fresh");
        fresh.last_activity = now_micros();
        store.put_thread(&fresh).expect("put fresh");

        let archived = store
            .archive_idle(std::time::Duration::from_secs(14 * 24 * 60 * 60))
            .expect("archive");
        assert_eq!(archived, 1, "only the stale thread archives");
        assert!(!store.get_thread(&thread_id("stale")).unwrap().unwrap().active);
        assert!(store.get_thread(&thread_id("fresh")).unwrap().unwrap().active);

        // Idempotent: an already-archived thread does not re-count.
        assert_eq!(
            store
                .archive_idle(std::time::Duration::from_secs(14 * 24 * 60 * 60))
                .expect("archive again"),
            0
        );
    }

    #[test]
    fn membership_round_trips() {
        let (_d, store) = temp_store();
        let thread = thread_id("th-1");
        let agent = agent_id("agent-1");
        store
            .add_member(&Membership {
                agent_id: agent.clone(),
                thread: thread.clone(),
                created_at: now_micros(),
            })
            .expect("add");
        assert!(store.is_member(&thread, &agent).expect("is_member"));
        assert_eq!(store.members(&thread).expect("members"), vec![agent.clone()]);
        assert_eq!(store.threads_for_agent(&agent).expect("threads"), vec![thread.clone()]);
        store.remove_member(&thread, &agent).expect("remove");
        assert!(store.members(&thread).expect("members").is_empty());
        assert!(!store.is_member(&thread, &agent).expect("is_member"));
    }

    #[test]
    fn read_cursor_is_monotonic() {
        let (_d, store) = temp_store();
        let thread = thread_id("th-1");
        let agent = agent_id("agent-1");
        assert_eq!(store.read_cursor(&agent, &thread).expect("read"), 0);
        store.set_read_cursor(&agent, &thread, 5).expect("set");
        assert_eq!(store.read_cursor(&agent, &thread).expect("read"), 5);
        store.set_read_cursor(&agent, &thread, 3).expect("set");
        assert_eq!(store.read_cursor(&agent, &thread).expect("read"), 5);
    }

    #[test]
    fn resolve_ids_maps_each_id_to_its_thread_and_seq() {
        let (_d, store) = temp_store();
        let thread_a = thread_id("th-a");
        let thread_b = thread_id("th-b");
        let mk = |store: &CommsStore, thread: &ThreadId, id: &str| {
            let body = id.as_bytes().to_vec();
            let meta = build_meta(
                id.to_string(),
                thread.clone(),
                agent_id("a"),
                id.to_string(),
                vec![],
                None,
                &body,
            );
            store.post(thread, meta, MessageBody(body)).expect("post").0
        };
        let s_a1 = mk(&store, &thread_a, "m-a1");
        let _s_a2 = mk(&store, &thread_a, "m-a2");
        let s_b1 = mk(&store, &thread_b, "m-b1");

        let mut got = store
            .resolve_ids(&["m-a1".to_string(), "m-b1".to_string(), "ghost".to_string()])
            .expect("resolve_ids");
        got.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(
            got,
            vec![
                ("m-a1".to_string(), thread_a.clone(), s_a1),
                ("m-b1".to_string(), thread_b.clone(), s_b1),
            ]
        );
        assert!(store.resolve_ids(&[]).expect("resolve_ids").is_empty());
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
        assert_eq!(store.get_agent(&agent_id("agent-1")).expect("get"), Some(rec));
    }
}
