//! The store against the conformance suite, and what is true of the store only: what survives a
//! reopen, how versions are handled, concurrency, when a call writes and the bounded sweep.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use tempfile::TempDir;

use super::open::Migration;
use super::*;
use crate::comms::v2::api::{
    COMMS_STORE_VER, InboxSelect, MESSAGE_TTL, Mark, MessageId, PostTarget, REPLAY_TTL, ReplayKey,
    SEEN_REFRESH_INTERVAL, SEEN_TTL, ThreadAction,
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

    /// When `name`'s seen record was last written, as stored.
    fn recorded_seen(&self, name: &str) -> Option<i64> {
        self.reader(self.clock.now_micros())
            .stored_seen(&caller(name).actor)
            .expect("seen record")
    }

    /// Rows physically in each keyspace but `meta`, in the order of the module's table.
    fn stored_rows(&self) -> [usize; 10] {
        let ks = &self.ks;
        [
            &ks.threads,
            &ks.messages,
            &ks.bodies,
            &ks.post_order,
            &ks.members,
            &ks.member_threads,
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
        let store = self.store();
        let reader = store.reader(store.clock.now_micros());
        reader.thread(thread).expect("thread")?;
        let position = reader.position(thread, actor).expect("member row")?;
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

    /// Threads physically stored once the sweep has nothing left to remove, so every scenario
    /// that counts threads also proves that expired ones are deleted, not only hidden.
    fn thread_count(&self) -> usize {
        ConformanceHarness::sweep(self);
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

#[test]
fn timestamps_keep_rising_when_the_clock_steps_back_less_than_the_tolerance() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness.clock.0.store(START_MICROS - 5_000_000, Ordering::SeqCst);
    say(&harness, "a", &created.thread, "second");
    say(&harness, "a", &created.thread, "third");

    let reply = harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");

    let times: Vec<i64> = reply.messages.iter().map(|message| message.ts_micros).collect();
    assert_eq!(times, [START_MICROS, START_MICROS + 1, START_MICROS + 2]);
}

#[test]
fn history_pages_by_cursor_through_a_thread_that_dropped_its_oldest_messages() {
    use crate::comms::v2::api::MAX_MESSAGES_PER_THREAD;
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    let elsewhere = start(&harness, "a", "elsewhere", &["b"]);
    for n in 0..MAX_MESSAGES_PER_THREAD + 4 {
        say(&harness, "a", &created.thread, &format!("m{n}"));
    }
    say(&harness, "a", &elsewhere.thread, "last");

    let mut ids: Vec<MessageId> = Vec::new();
    let mut cursor = None;
    loop {
        let page = harness
            .inbox(
                &caller("b"),
                InboxBody {
                    limit: Some(100),
                    cursor: cursor.take(),
                    ..history()
                },
            )
            .expect("inbox");
        ids.extend(page.messages.into_iter().map(|message| message.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    let mut expected = vec![elsewhere.id.clone()];
    expected
        .extend((6..=MAX_MESSAGES_PER_THREAD + 5).map(|seq| MessageId::new(created.thread.clone(), seq).expect("id")));
    expected.push(MessageId::new(elsewhere.thread, 2).expect("id"));
    assert_eq!(ids, expected);
}

#[test]
fn reads_of_a_thread_far_past_its_cap_start_at_the_oldest_message_still_stored() {
    use crate::comms::v2::api::MAX_MESSAGES_PER_THREAD;
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b", "c"]);
    let early = say(&harness, "a", &created.thread, "early");
    let marked = harness.inbox(&caller("c"), InboxBody::default()).expect("inbox");
    assert_eq!(marked.read_to, [early.id]);
    for n in 0..MAX_MESSAGES_PER_THREAD + 300 {
        say(&harness, "a", &created.thread, &format!("m{n}"));
    }
    let head = MAX_MESSAGES_PER_THREAD + 302;
    let oldest = head - MAX_MESSAGES_PER_THREAD + 1;
    let id = |seq: u64| MessageId::new(created.thread.clone(), seq).expect("id");

    let store = harness.store();
    let reader = store.reader(store.clock.now_micros());
    let record = reader.thread(&created.thread).expect("read").expect("thread");
    assert_eq!(record.head, head);
    assert_eq!(
        (record.first_stored(1), record.first_stored(oldest + 5)),
        (oldest, oldest + 5)
    );
    assert!(reader.stored_meta(&created.thread, oldest).expect("read").is_some());
    assert!(reader.stored_meta(&created.thread, oldest - 1).expect("read").is_none());
    drop(reader);

    for (name, position) in [("b", 0), ("c", 2)] {
        let summary = &harness
            .threads_query(&caller(name), ThreadQuery::Describe(created.thread.clone()))
            .expect("describe")
            .threads[0];
        assert_eq!(
            (summary.head, summary.unread),
            (head, MAX_MESSAGES_PER_THREAD),
            "{name}"
        );
        let peeked = harness
            .inbox(
                &caller(name),
                InboxBody {
                    limit: Some(2),
                    ..peek()
                },
            )
            .expect("peek");
        let ids: Vec<MessageId> = peeked.messages.into_iter().map(|message| message.id).collect();
        assert_eq!(ids, [id(oldest), id(oldest + 1)], "{name}");
        let page_two = harness
            .inbox(
                &caller(name),
                InboxBody {
                    limit: Some(2),
                    cursor: peeked.next_cursor,
                    ..peek()
                },
            )
            .expect("peek");
        let ids: Vec<MessageId> = page_two.messages.into_iter().map(|message| message.id).collect();
        assert_eq!(ids, [id(oldest + 2), id(oldest + 3)], "{name}");
        assert_eq!(
            harness.position(&created.thread, &caller(name).actor),
            Some((position, position)),
            "{name}"
        );
    }

    let read = harness
        .inbox(
            &caller("c"),
            InboxBody {
                limit: Some(1),
                ..InboxBody::default()
            },
        )
        .expect("inbox");
    assert_eq!(read.read_to, [id(oldest)]);
    let summary = &harness
        .threads_query(&caller("c"), ThreadQuery::Describe(created.thread.clone()))
        .expect("describe")
        .threads[0];
    assert_eq!(summary.unread, MAX_MESSAGES_PER_THREAD - 1);
}

#[test]
fn thread_row_tracks_the_newest_message_of_everyone_but_its_author() {
    let (a, b, c) = (caller("a").actor, caller("b").actor, caller("c").actor);
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b", "c"]);
    let newest = |actors: [&Actor; 3]| {
        let store = harness.store();
        let record = store
            .reader(store.clock.now_micros())
            .thread(&created.thread)
            .expect("read")
            .expect("thread");
        actors.map(|actor| record.newest_from_others(actor))
    };

    assert_eq!(newest([&a, &b, &c]), [0, 1, 1]);
    say(&harness, "a", &created.thread, "2");
    assert_eq!(newest([&a, &b, &c]), [0, 2, 2]);
    say(&harness, "b", &created.thread, "3");
    assert_eq!(newest([&a, &b, &c]), [3, 2, 3]);
    say(&harness, "c", &created.thread, "4");
    say(&harness, "c", &created.thread, "5");
    assert_eq!(newest([&a, &b, &c]), [5, 5, 3]);
    say(&harness, "a", &created.thread, "6");
    assert_eq!(newest([&a, &b, &c]), [5, 6, 6]);
}
