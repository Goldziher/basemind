//! The daemon's half of agent comms v2 (ADR-0021): one [`FjallAgents`] store for every session,
//! the wait of `agent_inbox`, the relay's replays that arrive during one, the repository of a
//! root, and the count of live `serve` sessions per principal behind the shared-identity notice.
//!
//! The store never blocks and is synchronous; the hub runs it on the blocking pool for async
//! callers and wakes waiters through a `watch` of the store-wide post counter. It is opened on a
//! thread of its own, so a daemon whose store takes seconds to create answers everything else
//! meanwhile; an agent request waits for the open.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
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
use super::v2::store::{FjallAgents, STORE_DIR};

/// Reads a wait may make back to back, without waiting, while each walks only the caller's own
/// posts: a read that stopped at its scan budget passes them, so a few reach what follows.
const MAX_READS_WITHOUT_WAITING: usize = 8;

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

/// Where the first of a relayed read's requests publishes its result for a replay that joins it.
type WaitResult = watch::Receiver<Option<Result<InboxReply, CommsError>>>;

/// The store, or why it did not open, once its open has finished.
#[derive(Default)]
struct StoreSlot {
    value: OnceLock<Result<Arc<FjallAgents>, CommsError>>,
    lock: Mutex<()>,
    ready: Condvar,
}

impl StoreSlot {
    fn set(&self, value: Result<Arc<FjallAgents>, CommsError>) {
        let _held = lock(&self.lock);
        let _ = self.value.set(value);
        self.ready.notify_all();
    }

    /// The store, waiting for its open to finish. For a blocking thread only.
    fn wait(&self) -> Result<Arc<FjallAgents>, CommsError> {
        let mut held = lock(&self.lock);
        loop {
            if let Some(value) = self.value.get() {
                return value.clone();
            }
            held = self.ready.wait(held).unwrap_or_else(PoisonError::into_inner);
        }
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
                let store = FjallAgents::open(&dir).map(Arc::new);
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
        }
    }

    /// The store, waiting for its open. Called on blocking threads only.
    fn store(&self) -> Result<Arc<FjallAgents>, CommsError> {
        self.store.wait()
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
        let hub = Arc::clone(self);
        joined(tokio::task::spawn_blocking(move || hub.ops().post(&caller, body)).await)
    }

    /// Query threads, on the blocking pool.
    pub async fn threads_query(
        self: &Arc<Self>,
        caller: Caller,
        query: ThreadQuery,
    ) -> Result<ThreadsReply, CommsError> {
        let hub = Arc::clone(self);
        joined(tokio::task::spawn_blocking(move || hub.ops().threads_query(&caller, query)).await)
    }

    /// Change a thread, on the blocking pool.
    pub async fn threads_update(
        self: &Arc<Self>,
        caller: Caller,
        update: ThreadUpdate,
    ) -> Result<ThreadsReply, CommsError> {
        let hub = Arc::clone(self);
        joined(tokio::task::spawn_blocking(move || hub.ops().threads_update(&caller, update)).await)
    }

    async fn inbox_now(self: &Arc<Self>, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        let (hub, caller) = (Arc::clone(self), caller.clone());
        joined(tokio::task::spawn_blocking(move || hub.ops().inbox(&caller, body)).await)
    }

    /// Read the inbox, waiting up to `wait` while nothing is unread. `gone` is cancelled when the
    /// client has gone: a waiting read checks it immediately before it marks anything read, so a
    /// waiter nobody is listening to advances nothing.
    ///
    /// A relayed marking read is registered under its replay key before it waits, and a replay of
    /// it that arrives meanwhile gets the same reply.
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
            return self.wait_and_read(&caller, body, kind, wait, gone).await;
        };
        let joining = lock(&self.waits).get(&key).cloned();
        if let Some(mut first) = joining {
            loop {
                if let Some(result) = first.borrow_and_update().clone() {
                    return result;
                }
                if first.changed().await.is_err() {
                    return self.inbox_now(&caller, body).await;
                }
            }
        }
        let (done, receiver) = watch::channel(None);
        lock(&self.waits).insert(key.clone(), receiver);
        let result = self.wait_and_read(&caller, body, kind, wait, gone).await;
        done.send_replace(Some(result.clone()));
        lock(&self.waits).remove(&key);
        result
    }

    async fn wait_and_read(
        self: &Arc<Self>,
        caller: &Caller,
        body: InboxBody,
        kind: ReadKind,
        wait: Duration,
        gone: &CancellationToken,
    ) -> Result<InboxReply, CommsError> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut arrivals = self.arrivals.subscribe();
        let probe = InboxBody {
            mark: Mark::Peek,
            limit: Some(1),
            cursor: None,
            ..body.clone()
        };
        let mut waited = false;
        let mut reads = 0;
        loop {
            arrivals.borrow_and_update();
            let reply = if kind == ReadKind::Marking {
                let found = self.inbox_now(caller, probe.clone()).await?;
                if found.messages.is_empty() && !found.more {
                    found
                } else if gone.is_cancelled() {
                    return Ok(self.ended(caller, WaitOutcome::TimedOut));
                } else {
                    self.inbox_now(caller, body.clone()).await?
                }
            } else {
                self.inbox_now(caller, body.clone()).await?
            };
            if !reply.messages.is_empty() {
                return Ok(InboxReply {
                    wait: if waited {
                        WaitOutcome::Arrived
                    } else {
                        WaitOutcome::NotWaited
                    },
                    ..reply
                });
            }
            reads += 1;
            if kind == ReadKind::Marking && reply.more && reads < MAX_READS_WITHOUT_WAITING {
                continue;
            }
            reads = 0;
            waited = true;
            let woken = tokio::select! {
                changed = arrivals.changed() => changed.is_ok(),
                () = gone.cancelled() => false,
                () = tokio::time::sleep_until(deadline) => false,
            };
            if !woken {
                return Ok(self.ended(caller, WaitOutcome::TimedOut));
            }
        }
    }

    fn ended(&self, caller: &Caller, wait: WaitOutcome) -> InboxReply {
        let mut reply = rules::inbox_reply(Vec::new(), Vec::new(), false, None);
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
