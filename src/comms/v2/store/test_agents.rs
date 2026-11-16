//! [`TestAgents`]: the Fjall store in a temporary directory, on a clock and a thread-id source a
//! test controls, for the tests of anything built on [`AgentOps`]: the tools, the CLI, the front.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tempfile::TempDir;

use super::{Clock, FjallAgents, StoreOptions, ThreadIds};
use crate::comms::v2::api::{
    Actor, AgentOps, Caller, CommsError, InboxBody, InboxReply, PostBody, PostReply, ThreadId, ThreadQuery,
    ThreadUpdate, ThreadsReply,
};
use crate::comms::v2::conformance::ConformanceHarness;
use crate::comms::v2::rules;

/// What a [`TestAgents`] clock reads until a test moves it: 2023-11-14T22:13:20Z.
pub const TEST_EPOCH_MICROS: i64 = 1_700_000_000_000_000;

/// A clock a test moves.
struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now_micros(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Thread ids from a queue a test fills, then from a counter from 0.
#[derive(Clone, Default)]
struct QueuedIds(Arc<Mutex<(VecDeque<u32>, u32)>>);

impl ThreadIds for QueuedIds {
    fn draw(&mut self) -> Result<u32, CommsError> {
        let mut ids = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(match ids.0.pop_front() {
            Some(bits) => bits,
            None => {
                let bits = ids.1;
                ids.1 = ids.1.wrapping_add(1);
                bits
            }
        })
    }
}

/// The store, opened fresh in a temporary directory that lives as long as it does.
pub struct TestAgents {
    store: FjallAgents,
    clock: Arc<TestClock>,
    ids: QueuedIds,
    _dir: TempDir,
}

impl Default for TestAgents {
    fn default() -> Self {
        Self::new()
    }
}

impl TestAgents {
    /// An empty store, the clock at [`TEST_EPOCH_MICROS`].
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let clock = Arc::new(TestClock(AtomicI64::new(TEST_EPOCH_MICROS)));
        let ids = QueuedIds::default();
        let store = FjallAgents::open_with(
            dir.path(),
            StoreOptions {
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
                thread_ids: Box::new(ids.clone()),
            },
        )
        .expect("open the store");
        Self {
            store,
            clock,
            ids,
            _dir: dir,
        }
    }

    /// Draw the next thread ids from `bits` before falling back to the counter. Repeating a value
    /// reproduces an id collision.
    pub fn queue_thread_bits(&self, bits: impl IntoIterator<Item = u32>) {
        self.ids.0.lock().unwrap_or_else(PoisonError::into_inner).0.extend(bits);
    }
}

impl AgentOps for TestAgents {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        self.store.post(caller, body)
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        self.store.inbox(caller, body)
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        self.store.threads_query(caller, query)
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        self.store.threads_update(caller, update)
    }

    fn arrivals(&self) -> u64 {
        self.store.arrivals()
    }
}

impl ConformanceHarness for TestAgents {
    fn advance(&self, by: Duration) {
        self.clock.0.fetch_add(rules::ttl_micros(by), Ordering::SeqCst);
    }

    fn rewind(&self, by: Duration) {
        self.clock.0.fetch_sub(rules::ttl_micros(by), Ordering::SeqCst);
    }

    fn sweep(&self) {
        if self.store.check_writable().is_ok() {
            while self.store.sweep().expect("sweep").more {}
        }
    }

    fn position(&self, thread: &ThreadId, actor: &Actor) -> Option<(u64, u64)> {
        let reader = super::records::Reader::new(&self.store.ks, self.store.db.snapshot());
        let (position, _) = reader.position(thread, actor).expect("position")?;
        Some((position.read_seq, position.notified_seq))
    }

    fn set_store_version(&self, version: u32) {
        self.store
            .ks
            .meta
            .insert(super::META_STORE_VER, version.to_be_bytes())
            .expect("write the version");
        self.store.store_ver.store(version, Ordering::Release);
    }

    fn thread_count(&self) -> usize {
        self.store.ks.threads.len().expect("count threads")
    }
}
