//! The daemon's half of agent comms v2 (ADR-0021): one [`FjallAgents`] store for every session,
//! the wait of `agent_inbox`, the relay's replays that arrive during one, the repository of a
//! root, and the count of live `serve` sessions per principal behind the shared-identity notice.
//!
//! The store never blocks and is synchronous; the hub runs it on the blocking pool for async
//! callers and wakes waiters through a `watch` of the store-wide post counter. A wait itself is
//! async and holds no blocking thread. The store is opened on a thread of its own, so a daemon
//! whose store takes seconds to create answers everything else meanwhile; an agent request awaits
//! the open without holding a blocking thread either.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::v2::api::{
    Actor, AgentOps, Caller, CommsError, InboxBody, InboxReply, Mark, PostBody, PostReply, Principal, ReadKind,
    RepoKey, ThreadId, ThreadQuery, ThreadUpdate, ThreadsReply, WaitOutcome, shared_identity_notice,
};
use super::v2::rules;
use super::v2::store::{FjallAgents, STORE_DIR, is_locked};

/// Reads a wait may make back to back, without waiting, while each walks only the caller's own
/// posts: a read that stopped at its scan budget passes them, so a few reach what follows.
const MAX_READS_WITHOUT_WAITING: usize = 8;

/// How long the open keeps trying a store another process holds. A daemon taking over from an
/// older one finds its store held until that one has drained and exited.
const LOCKED_STORE_PATIENCE: Duration = Duration::from_secs(120);

/// The longest pause between two tries of a held store.
const LOCKED_STORE_MAX_PAUSE: Duration = Duration::from_secs(2);

/// Open the store in `dir`, trying again with a growing pause while another process holds it, for
/// up to [`LOCKED_STORE_PATIENCE`]. Any other failure is final at once.
fn open_waiting_for_lock(dir: &Path) -> Result<FjallAgents, CommsError> {
    let started = std::time::Instant::now();
    let mut pause = Duration::from_millis(50);
    loop {
        match FjallAgents::open(dir) {
            Err(error) if is_locked(&error) && started.elapsed() < LOCKED_STORE_PATIENCE => {
                tracing::info!(%error, "comms: the agent store is held by another process; trying again");
                std::thread::sleep(pause);
                pause = (pause * 2).min(LOCKED_STORE_MAX_PAUSE);
            }
            result => return result,
        }
    }
}

/// What `basemind agent status` reports about the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStoreStatus {
    /// The store's directory.
    pub dir: PathBuf,
    /// `opening`, `open`, `newer` (read-only: written by a newer basemind) or `unreadable`.
    pub state: String,
    /// The layout version on disk, when the store opened.
    #[serde(default)]
    pub store_version: Option<u32>,
    /// Why the store could not be opened.
    #[serde(default)]
    pub error: Option<String>,
    /// Posts stored so far: the store-wide post counter.
    #[serde(default)]
    pub posts: u64,
    /// Live `serve` sessions of the asking principal.
    #[serde(default)]
    pub sessions: usize,
    /// The repository the asking connection's root belongs to.
    #[serde(default)]
    pub repo: Option<RepoKey>,
}

/// What `basemind agent cleanup` did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCleanupReport {
    /// Threads archived now.
    #[serde(default)]
    pub archived: Vec<ThreadId>,
    /// Threads named that were archived already.
    #[serde(default)]
    pub already_archived: Vec<ThreadId>,
    /// Records the sweep removed: messages, threads, replay entries, seen records.
    #[serde(default)]
    pub swept: usize,
    /// Whether the sweep stopped at its budget with more to remove.
    #[serde(default)]
    pub more: bool,
}

/// What the first of a relayed read's requests publishes for a replay that joins it.
#[derive(Clone)]
enum Published {
    /// Still waiting.
    Pending,
    /// Its reply, which the replay returns as its own.
    Done(Result<InboxReply, CommsError>),
    /// Its client went away before anything was read: the replay waits on its own.
    Abandoned,
}

/// Where the first of a relayed read's requests publishes its result.
type WaitResult = watch::Receiver<Published>;

/// The store, or why it did not open, once its open has finished.
struct StoreSlot {
    value: OnceLock<Result<Arc<FjallAgents>, CommsError>>,
    ready: watch::Sender<bool>,
}

impl Default for StoreSlot {
    fn default() -> Self {
        Self {
            value: OnceLock::new(),
            ready: watch::Sender::new(false),
        }
    }
}

impl StoreSlot {
    fn set(&self, value: Result<Arc<FjallAgents>, CommsError>) {
        let _ = self.value.set(value);
        self.ready.send_replace(true);
    }

    /// The store, once its open has finished. Holds no thread while it waits.
    async fn opened(&self) -> Result<Arc<FjallAgents>, CommsError> {
        let mut ready = self.ready.subscribe();
        // The sender lives as long as the slot, so the wait ends only when the open has.
        let _ = ready.wait_for(|opened| *opened).await;
        self.get()
    }

    /// The store, or a refusal while it is still opening. Callers await [`Self::opened`] first.
    fn get(&self) -> Result<Arc<FjallAgents>, CommsError> {
        self.value.get().cloned().unwrap_or_else(|| {
            Err(CommsError::Backend {
                detail: "the agent store is still opening; try again in a moment".to_string(),
            })
        })
    }
}

/// The first of a relayed read's requests, registered under its replay key. However the read ends
/// (a return, a panic, or its future dropped), dropping this removes the key, and a read that
/// published nothing tells a joined replay it was abandoned, so the replay never waits on a sender
/// that is gone.
struct FirstRead<'a> {
    waits: &'a Mutex<AHashMap<Vec<u8>, WaitResult>>,
    key: Vec<u8>,
    done: watch::Sender<Published>,
    mine: WaitResult,
}

impl FirstRead<'_> {
    /// Remove the key, unless a later first read registered it again.
    fn unregister(&self) {
        let mut waits = lock(self.waits);
        if waits
            .get(&self.key)
            .is_some_and(|registered| registered.same_channel(&self.mine))
        {
            waits.remove(&self.key);
        }
    }

    /// Hand the result to a joined replay. The key goes first, so a replay that arrives after this
    /// starts on its own instead of finding the finished read.
    fn publish(self, published: Published) {
        self.unregister();
        self.done.send_replace(published);
    }
}

impl Drop for FirstRead<'_> {
    fn drop(&mut self) {
        self.unregister();
        self.done.send_if_modified(|published| {
            let pending = matches!(published, Published::Pending);
            if pending {
                *published = Published::Abandoned;
            }
            pending
        });
    }
}

/// The agent store as the daemon serves it.
pub struct AgentHub {
    dir: PathBuf,
    store: Arc<StoreSlot>,
    arrivals: watch::Sender<u64>,
    waits: Mutex<AHashMap<Vec<u8>, WaitResult>>,
    repos: Mutex<AHashMap<PathBuf, RepoKey>>,
    sessions: Mutex<AHashMap<Principal, usize>>,
    /// Cancelled when the daemon drains: every wait ends at once.
    shutdown: CancellationToken,
}

/// One live `serve` session of a principal, counted while it lives.
pub struct ServeSession {
    hub: Arc<AgentHub>,
    principal: Principal,
}

impl Drop for ServeSession {
    fn drop(&mut self) {
        let mut sessions = lock(&self.hub.sessions);
        if let Some(count) = sessions.get_mut(&self.principal) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                sessions.remove(&self.principal);
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A store failure the blocking pool could not even run.
fn joined<T>(result: Result<Result<T, CommsError>, tokio::task::JoinError>) -> Result<T, CommsError> {
    match result {
        Ok(result) => result,
        Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
        Err(join) => Err(CommsError::Backend {
            detail: format!("the comms store call did not run: {join}"),
        }),
    }
}

impl AgentHub {
    /// Open the store in `generation_dir` (`<comms dir>/g<N>`). A store that cannot be opened is
    /// left as it is: every agent request then answers with the reason, and the daemon still
    /// serves everything else.
    pub fn open(generation_dir: &Path) -> Self {
        Self::open_after(generation_dir, Duration::ZERO)
    }

    /// [`AgentHub::open`], starting the open after `delay`: the daemon's test hook for a slow
    /// store.
    pub fn open_after(generation_dir: &Path, delay: Duration) -> Self {
        let hub = Self::empty(generation_dir.join(STORE_DIR));
        let (slot, dir) = (Arc::clone(&hub.store), generation_dir.to_path_buf());
        let opener = std::thread::Builder::new()
            .name("agent-store-open".to_string())
            .spawn(move || {
                std::thread::sleep(delay);
                let store = open_waiting_for_lock(&dir).map(Arc::new);
                if let Err(error) = &store {
                    tracing::error!(%error, "comms: the agent store did not open; agent tools will refuse until it is fixed");
                }
                slot.set(store);
            });
        if let Err(error) = opener {
            hub.store.set(Err(CommsError::Backend {
                detail: format!("could not start opening the agent store: {error}"),
            }));
        }
        hub
    }

    /// A hub over a store opened elsewhere (tests open one with their own clock).
    pub fn with_store(dir: PathBuf, store: Result<Arc<FjallAgents>, CommsError>) -> Self {
        let hub = Self::empty(dir);
        hub.store.set(store);
        hub
    }

    fn empty(dir: PathBuf) -> Self {
        Self {
            dir,
            store: Arc::new(StoreSlot::default()),
            arrivals: watch::Sender::new(0),
            waits: Mutex::new(AHashMap::new()),
            repos: Mutex::new(AHashMap::new()),
            sessions: Mutex::new(AHashMap::new()),
            shutdown: CancellationToken::new(),
        }
    }

    /// The store, once its open has finished.
    fn store(&self) -> Result<Arc<FjallAgents>, CommsError> {
        self.store.get()
    }

    /// Wait for the store's open to finish, without holding a thread. Every async entry point
    /// awaits it before it hands work to the blocking pool, so a blocking thread never waits on
    /// the open.
    pub async fn opened(&self) {
        let _ = self.store.opened().await;
    }

    /// End every wait now and refuse to start new ones: the daemon is draining.
    pub fn shut_down(&self) {
        self.shutdown.cancel();
    }

    /// The repository `root` belongs to: its registry id (the normalised `origin` remote) when it
    /// is a git checkout with one, else `path:<root>`. Cached per root.
    pub fn repo_for(&self, root: &Path) -> Result<RepoKey, CommsError> {
        if let Some(repo) = lock(&self.repos).get(root) {
            return Ok(repo.clone());
        }
        let key = match crate::git::Repo::discover(root) {
            Ok(repository) => crate::git::scope_key(&repository),
            Err(_) => format!("{}{}", super::v2::api::PATH_REPO_PREFIX, root.display()),
        };
        let repo = RepoKey::parse(key)?;
        lock(&self.repos).insert(root.to_path_buf(), repo.clone());
        Ok(repo)
    }

    /// Count a live `serve` session of `principal` until the returned guard drops.
    pub fn serve_session(self: &Arc<Self>, principal: Principal) -> ServeSession {
        *lock(&self.sessions).entry(principal.clone()).or_default() += 1;
        ServeSession {
            hub: Arc::clone(self),
            principal,
        }
    }

    /// Live `serve` sessions of `principal`.
    pub fn sessions_of(&self, principal: &str) -> usize {
        lock(&self.sessions)
            .iter()
            .find(|(live, _)| live.as_str() == principal)
            .map_or(0, |(_, count)| *count)
    }

    /// The daemon's notices for a reply to `actor`.
    pub fn notices_for(&self, actor: &Actor) -> Vec<String> {
        shared_identity_notice(actor, self.sessions_of(actor.principal()))
            .into_iter()
            .collect()
    }

    /// Wake every waiter: the post counter moved.
    fn posted(&self, store: &FjallAgents) {
        self.arrivals.send_replace(store.arrivals());
    }

    /// The store as a synchronous [`AgentOps`] for a caller on a blocking thread, with the daemon's
    /// notices added and waiters woken on a post.
    pub fn ops(&self) -> HubOps<'_> {
        HubOps { hub: self }
    }

    /// Post, on the blocking pool.
    pub async fn post(self: &Arc<Self>, caller: Caller, body: PostBody) -> Result<PostReply, CommsError> {
        self.opened().await;
        let hub = Arc::clone(self);
        joined(tokio::task::spawn_blocking(move || hub.ops().post(&caller, body)).await)
    }

    /// Query threads, on the blocking pool.
    pub async fn threads_query(
        self: &Arc<Self>,
        caller: Caller,
        query: ThreadQuery,
    ) -> Result<ThreadsReply, CommsError> {
        self.opened().await;
        let hub = Arc::clone(self);
        joined(tokio::task::spawn_blocking(move || hub.ops().threads_query(&caller, query)).await)
    }

    /// Change a thread, on the blocking pool.
    pub async fn threads_update(
        self: &Arc<Self>,
        caller: Caller,
        update: ThreadUpdate,
    ) -> Result<ThreadsReply, CommsError> {
        self.opened().await;
        let hub = Arc::clone(self);
        joined(tokio::task::spawn_blocking(move || hub.ops().threads_update(&caller, update)).await)
    }

    async fn inbox_now(self: &Arc<Self>, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        self.opened().await;
        let (hub, caller) = (Arc::clone(self), caller.clone());
        joined(tokio::task::spawn_blocking(move || hub.ops().inbox(&caller, body)).await)
    }

    /// Read the inbox, waiting up to `wait` while nothing is unread. `gone` is cancelled when the
    /// client has gone: a waiting read checks it immediately before it marks anything read, so a
    /// waiter nobody is listening to advances nothing. The wait holds no thread: only each read of
    /// the store runs on the blocking pool.
    ///
    /// A relayed marking read is registered under its replay key before it waits, and a replay of
    /// it that arrives meanwhile gets the same reply. A replay that arrives after the first has
    /// finished gets the ranges the store remembered for the key.
    pub async fn read(
        self: &Arc<Self>,
        caller: Caller,
        body: InboxBody,
        wait: Duration,
        gone: &CancellationToken,
    ) -> Result<InboxReply, CommsError> {
        let kind = body.kind()?;
        if wait.is_zero() {
            return self.inbox_now(&caller, body).await;
        }
        let key = caller
            .replay_key
            .as_ref()
            .filter(|_| kind == ReadKind::Marking)
            .map(|key| [caller.actor.as_str().as_bytes(), b"\0", key.as_str().as_bytes()].concat());
        let Some(key) = key else {
            return self.waited(&caller, body, kind, wait, gone).await;
        };
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let (registered, joining) = {
                let mut waits = lock(&self.waits);
                match waits.entry(key.clone()) {
                    std::collections::hash_map::Entry::Occupied(first) => (None, Some(first.get().clone())),
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        let (done, mine) = watch::channel(Published::Pending);
                        slot.insert(mine.clone());
                        let registered = FirstRead {
                            waits: &self.waits,
                            key: key.clone(),
                            done,
                            mine,
                        };
                        (Some(registered), None)
                    }
                }
            };
            if let Some(registered) = registered {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                let result = self.wait_and_read(&caller, body, kind, left, gone).await;
                return match result {
                    Ok(Some(reply)) => {
                        registered.publish(Published::Done(Ok(reply.clone())));
                        Ok(reply)
                    }
                    Ok(None) => {
                        registered.publish(Published::Abandoned);
                        Ok(self.ended(&caller, WaitOutcome::TimedOut, None))
                    }
                    Err(error) => {
                        registered.publish(Published::Done(Err(error.clone())));
                        Err(error)
                    }
                };
            }
            let Some(mut first) = joining else {
                unreachable!("a key is either joined or registered");
            };
            loop {
                let published = first.borrow_and_update().clone();
                match published {
                    Published::Done(result) => return result,
                    Published::Abandoned => break,
                    Published::Pending => {}
                }
                if first.changed().await.is_err() {
                    // Its sender is gone without a result: drop the stale entry, if it is still
                    // this one, and wait on our own.
                    let mut waits = lock(&self.waits);
                    if waits
                        .get(&key)
                        .is_some_and(|registered| registered.same_channel(&first))
                    {
                        waits.remove(&key);
                    }
                    break;
                }
            }
            if gone.is_cancelled() || tokio::time::Instant::now() >= deadline {
                return Ok(self.ended(&caller, WaitOutcome::TimedOut, None));
            }
        }
    }

    /// [`Self::wait_and_read`] for a read no replay can join: a client that went away ends it like
    /// a timeout.
    async fn waited(
        self: &Arc<Self>,
        caller: &Caller,
        body: InboxBody,
        kind: ReadKind,
        wait: Duration,
        gone: &CancellationToken,
    ) -> Result<InboxReply, CommsError> {
        Ok(self
            .wait_and_read(caller, body, kind, wait, gone)
            .await?
            .unwrap_or_else(|| self.ended(caller, WaitOutcome::TimedOut, None)))
    }

    /// Read, and while nothing comes back, wait for a post and read again until `wait` has passed.
    /// `None` when the client went away before anything was read.
    async fn wait_and_read(
        self: &Arc<Self>,
        caller: &Caller,
        body: InboxBody,
        kind: ReadKind,
        wait: Duration,
        gone: &CancellationToken,
    ) -> Result<Option<InboxReply>, CommsError> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut arrivals = self.arrivals.subscribe();
        let probe = InboxBody {
            mark: Mark::Peek,
            limit: Some(1),
            cursor: None,
            ..body.clone()
        };
        // A relayed marking read goes to the store at once: the store answers a replay of a read
        // that already delivered with what it delivered, which a peek would not see.
        let mut probe_first = caller.replay_key.is_none();
        let mut waited = false;
        let mut reads = 0;
        loop {
            arrivals.borrow_and_update();
            let reply = if kind == ReadKind::Marking {
                let found = if probe_first {
                    Some(self.inbox_now(caller, probe.clone()).await?)
                } else {
                    None
                };
                probe_first = true;
                match found {
                    Some(found) if found.messages.is_empty() && !found.more => found,
                    _ if gone.is_cancelled() => return Ok(None),
                    _ => self.inbox_now(caller, body.clone()).await?,
                }
            } else {
                self.inbox_now(caller, body.clone()).await?
            };
            let outcome = if waited {
                WaitOutcome::Arrived
            } else {
                WaitOutcome::NotWaited
            };
            if !reply.messages.is_empty() {
                return Ok(Some(InboxReply { wait: outcome, ..reply }));
            }
            // A peek that stopped at its scan budget hands back a cursor past what it walked;
            // reading again from the same place would walk the same rows, so the caller continues.
            if kind != ReadKind::Marking && reply.more {
                return Ok(Some(InboxReply { wait: outcome, ..reply }));
            }
            reads += 1;
            if kind == ReadKind::Marking && reply.more && reads < MAX_READS_WITHOUT_WAITING {
                continue;
            }
            reads = 0;
            waited = true;
            let woken = tokio::select! {
                changed = arrivals.changed() => changed.is_ok(),
                () = gone.cancelled() => return Ok(None),
                () = self.shutdown.cancelled() => false,
                () = tokio::time::sleep_until(deadline) => false,
            };
            if !woken {
                return Ok(Some(self.ended(caller, WaitOutcome::TimedOut, Some(reply))));
            }
        }
    }

    /// The reply of a wait that ended with nothing to return, carrying whether the last read left
    /// more behind and where it stopped.
    fn ended(&self, caller: &Caller, wait: WaitOutcome, last: Option<InboxReply>) -> InboxReply {
        let (more, next_cursor) = last.map_or((false, None), |last| (last.more, last.next_cursor));
        let mut reply = rules::inbox_reply(Vec::new(), Vec::new(), more, next_cursor);
        reply.wait = wait;
        reply.notices = self.notices_for(&caller.actor);
        reply
    }

    /// The store's state, for `basemind agent status`.
    pub fn status(&self, principal: Option<&str>, repo: Option<RepoKey>) -> AgentStoreStatus {
        let sessions = principal.map_or(0, |principal| self.sessions_of(principal));
        match self.store.value.get() {
            None => AgentStoreStatus {
                dir: self.dir.clone(),
                state: "opening".to_string(),
                store_version: None,
                error: None,
                posts: 0,
                sessions,
                repo,
            },
            Some(Ok(store)) => {
                let version = store.store_version();
                let state = if rules::check_writable(version).is_ok() {
                    "open"
                } else {
                    "newer"
                };
                AgentStoreStatus {
                    dir: self.dir.clone(),
                    state: state.to_string(),
                    store_version: Some(version),
                    error: None,
                    posts: store.arrivals(),
                    sessions,
                    repo,
                }
            }
            Some(Err(error)) => AgentStoreStatus {
                dir: self.dir.clone(),
                state: "unreadable".to_string(),
                store_version: None,
                error: Some(error.to_string()),
                posts: 0,
                sessions,
                repo,
            },
        }
    }

    /// Archive `threads` whoever created them, then sweep once: the operator's `agent cleanup`.
    pub async fn cleanup(self: &Arc<Self>, threads: Vec<ThreadId>) -> Result<AgentCleanupReport, CommsError> {
        self.opened().await;
        let hub = Arc::clone(self);
        joined(
            tokio::task::spawn_blocking(move || {
                let store = hub.store()?;
                let mut report = AgentCleanupReport::default();
                for thread in threads {
                    if store.archive_any(&thread)? {
                        report.archived.push(thread);
                    } else {
                        report.already_archived.push(thread);
                    }
                }
                let swept = store.sweep()?;
                report.swept = swept.messages + swept.threads + swept.replays + swept.seen;
                report.more = swept.more;
                Ok(report)
            })
            .await,
        )
    }

    /// The periodic sweep and the flush to disk: the daemon's maintenance tick and its shutdown.
    pub async fn maintain(self: &Arc<Self>) {
        let Some(Ok(store)) = self.store.value.get().cloned() else {
            return;
        };
        let swept = tokio::task::spawn_blocking(move || {
            let swept = match rules::check_writable(store.store_version()) {
                Ok(()) => store.sweep().map(Some),
                Err(_) => Ok(None),
            };
            (swept, store.flush())
        })
        .await;
        match swept {
            Ok((Ok(Some(report)), flushed)) => {
                tracing::debug!(?report, "comms: agent store swept");
                if let Err(error) = flushed {
                    tracing::warn!(%error, "comms: agent store flush failed");
                }
            }
            Ok((Ok(None), _)) => {}
            Ok((Err(error), _)) => tracing::warn!(%error, "comms: agent store sweep failed"),
            Err(error) => tracing::warn!(%error, "comms: agent store maintenance did not run"),
        }
    }
}

/// [`AgentHub`] as a synchronous [`AgentOps`]: the store, its waiters woken on a post, and the
/// daemon's notices on every reply.
pub struct HubOps<'a> {
    hub: &'a AgentHub,
}

impl AgentOps for HubOps<'_> {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        let store = self.hub.store()?;
        let mut reply = store.post(caller, body)?;
        self.hub.posted(&store);
        reply.notices.extend(self.hub.notices_for(&caller.actor));
        Ok(reply)
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        let mut reply = self.hub.store()?.inbox(caller, body)?;
        reply.notices.extend(self.hub.notices_for(&caller.actor));
        Ok(reply)
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        let mut reply = self.hub.store()?.threads_query(caller, query)?;
        reply.notices.extend(self.hub.notices_for(&caller.actor));
        Ok(reply)
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        let mut reply = self.hub.store()?.threads_update(caller, update)?;
        reply.notices.extend(self.hub.notices_for(&caller.actor));
        Ok(reply)
    }

    fn arrivals(&self) -> u64 {
        self.hub.store().map_or(0, |store| store.arrivals())
    }
}

#[cfg(test)]
#[path = "agents_hub_tests.rs"]
mod tests;
