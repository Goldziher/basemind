//! The store against the conformance suite, and what is true of the store only: what survives a
//! reopen, how versions are handled, concurrent posts, when a call writes and the bounded sweep.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use tempfile::TempDir;

use super::open::Migration;
use super::*;
use crate::comms::v2::api::{
    COMMS_STORE_VER, MESSAGE_TTL, Mark, PostTarget, REPLAY_TTL, ReplayKey, SEEN_REFRESH_INTERVAL, SEEN_TTL,
    ThreadAction,
};
use crate::comms::v2::conformance::{ConformanceHarness, Repo, default_caller};

mod concurrency;
mod lifecycle;
mod retention;
mod seen;

/// Where the test clock starts, in microseconds since the unix epoch: 2026-01-01T00:00:00Z.
const START_MICROS: i64 = 1_767_225_600_000_000;

/// A clock a test moves.
struct TestClock(AtomicI64);

impl TestClock {
    fn advance(&self, by: Duration) {
        self.0.fetch_add(rules::ttl_micros(by), Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_micros(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Thread ids from a list, then from a counter.
struct ListedIds {
    listed: std::vec::IntoIter<u32>,
    next: u32,
}

impl ThreadIds for ListedIds {
    fn draw(&mut self) -> Result<u32, CommsError> {
        Ok(self.listed.next().unwrap_or_else(|| {
            self.next += 1;
            self.next
        }))
    }
}

/// A store in a temporary directory, on a clock the test moves.
struct Harness {
    store: Option<FjallAgents>,
    clock: Arc<TestClock>,
    dir: TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::with_ids(Vec::new())
    }

    fn with_ids(listed: Vec<u32>) -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let clock = Arc::new(TestClock(AtomicI64::new(START_MICROS)));
        let store = FjallAgents::open_with(dir.path(), options(&clock, listed)).expect("open");
        Self {
            store: Some(store),
            clock,
            dir,
        }
    }

    fn store(&self) -> &FjallAgents {
        self.store.as_ref().expect("the store is open")
    }

    /// Drop the handle without calling [`FjallAgents::flush`]. A clean drop inside a live
    /// process: Fjall's own teardown still runs, so this is not what a killed daemon leaves.
    fn drop_handle(&mut self) {
        self.store = None;
    }

    fn reopen(&mut self) {
        self.reopen_migrating(&[]).expect("reopen");
    }

    fn reopen_migrating(&mut self, migrations: &[Migration]) -> Result<(), CommsError> {
        self.drop_handle();
        let store = FjallAgents::open_migrating(self.dir.path(), options(&self.clock, Vec::new()), migrations)?;
        self.store = Some(store);
        Ok(())
    }
}

fn options(clock: &Arc<TestClock>, listed: Vec<u32>) -> StoreOptions {
    StoreOptions {
        clock: Arc::clone(clock) as Arc<dyn Clock>,
        thread_ids: Box::new(ListedIds {
            listed: listed.into_iter(),
            next: 1000,
        }),
    }
}

impl FjallAgents {
    /// Write `version` as the layout version, the way another binary would have.
    fn force_store_version(&self, version: u32) {
        self.ks
            .meta
            .insert(META_STORE_VER, version.to_be_bytes())
            .expect("write the version");
        self.store_ver.store(version, Ordering::Release);
    }

    fn reader(&self) -> Reader<'_> {
        Reader::new(&self.ks, self.db.snapshot())
    }

    /// When `name`'s seen record was last written.
    fn recorded_seen(&self, name: &str) -> Option<i64> {
        self.reader().seen(&caller(name).actor).expect("seen record")
    }

    /// Rows in each keyspace but `meta`, in the order of the module's table.
    fn stored_rows(&self) -> [usize; 12] {
        let ks = &self.ks;
        [
            &ks.threads,
            &ks.messages,
            &ks.bodies,
            &ks.post_order,
            &ks.members,
            &ks.member_threads,
            &ks.positions,
            &ks.actor_threads,
            &ks.repo_threads,
            &ks.replays,
            &ks.seen,
            &ks.expiry,
        ]
        .map(|keyspace| keyspace.len().expect("count rows"))
    }
}

impl AgentOps for Harness {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        self.store().post(caller, body)
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        self.store().inbox(caller, body)
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        self.store().threads_query(caller, query)
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        self.store().threads_update(caller, update)
    }

    fn arrivals(&self) -> u64 {
        self.store().arrivals()
    }
}

impl ConformanceHarness for Harness {
    fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    fn rewind(&self, by: Duration) {
        self.clock.0.fetch_sub(rules::ttl_micros(by), Ordering::SeqCst);
    }

    fn position(&self, thread: &ThreadId, actor: &Actor) -> Option<(u64, u64)> {
        let (position, _) = self.store().reader().position(thread, actor).expect("position")?;
        Some((position.read_seq, position.notified_seq))
    }

    fn set_store_version(&self, version: u32) {
        self.store().force_store_version(version);
    }

    fn sweep(&self) {
        let store = self.store();
        if store.check_writable().is_ok() {
            while store.sweep().expect("sweep").more {}
        }
    }

    fn thread_count(&self) -> usize {
        self.store().ks.threads.len().expect("count threads")
    }
}

mod conformance {
    crate::conformance_tests!(super::Harness::new());
}

fn caller(name: &str) -> Caller {
    default_caller(name, Repo::Home)
}

fn keyed(name: &str, key: &str) -> Caller {
    caller(name).with_replay_key(ReplayKey::parse(key).expect("replay key"))
}

fn start(harness: &Harness, creator: &str, subject: &str, to: &[&str]) -> PostReply {
    let body = PostBody {
        target: PostTarget::NewThread {
            subject: subject.to_string(),
            to: to.iter().map(|name| caller(name).actor).collect(),
            paths: Vec::new(),
        },
        body: "first".to_string(),
    };
    harness.post(&caller(creator), body).expect("thread created")
}

fn into(thread: &ThreadId, body: &str) -> PostBody {
    PostBody {
        target: PostTarget::Thread(thread.clone()),
        body: body.to_string(),
    }
}

fn say(harness: &Harness, name: &str, thread: &ThreadId, body: &str) -> PostReply {
    harness.post(&caller(name), into(thread, body)).expect("post")
}

fn history() -> InboxBody {
    InboxBody {
        unread: false,
        ..InboxBody::default()
    }
}

fn peek() -> InboxBody {
    InboxBody {
        mark: Mark::Peek,
        ..InboxBody::default()
    }
}

fn bodies(reply: &InboxReply) -> Vec<&str> {
    reply.messages.iter().map(|message| message.body.as_str()).collect()
}

#[test]
fn thread_ids_come_from_the_source_and_a_collision_is_drawn_again() {
    let harness = Harness::with_ids(vec![5, 5, 5, 6]);

    assert_eq!(start(&harness, "a", "one", &[]).thread, ThreadId::from_bits(5));
    assert_eq!(start(&harness, "a", "two", &[]).thread, ThreadId::from_bits(6));
}

#[test]
fn source_that_only_repeats_a_taken_id_fails_the_post_and_stores_nothing() {
    let harness = Harness::with_ids(vec![9; MAX_THREAD_ID_DRAWS + 1]);
    start(&harness, "a", "one", &[]);
    let before = (harness.arrivals(), harness.store().stored_rows());

    let refused = harness
        .post(
            &caller("a"),
            PostBody {
                target: PostTarget::NewThread {
                    subject: "two".to_string(),
                    to: Vec::new(),
                    paths: Vec::new(),
                },
                body: "first".to_string(),
            },
        )
        .expect_err("no free id");

    assert!(matches!(refused, CommsError::Backend { .. }), "{refused:?}");
    assert_eq!((harness.arrivals(), harness.store().stored_rows()), before);
}

#[test]
fn random_thread_ids_stay_inside_the_id_space() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = FjallAgents::open(dir.path()).expect("open");
    let body = PostBody {
        target: PostTarget::NewThread {
            subject: "topic".to_string(),
            to: Vec::new(),
            paths: Vec::new(),
        },
        body: "first".to_string(),
    };

    let first = store.post(&caller("a"), body.clone()).expect("post");
    let second = store.post(&caller("a"), body).expect("post");

    assert_ne!(first.thread, second.thread);
    assert_eq!(
        store
            .threads_query(&caller("a"), ThreadQuery::default())
            .expect("threads")
            .threads
            .len(),
        2
    );
}
