//! Posts, reads that mark, reads that do not, thread creation, leaving and the sweep, all at
//! once on one store.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

use super::records::MessageMeta;
use super::*;

const POSTERS: usize = 6;
const POSTS_EACH: usize = 30;
const LEAVERS: usize = 3;
const READS_BEFORE_LEAVING: usize = 5;
const CREATORS: usize = 2;
const THREADS_EACH: usize = 8;
/// Agents that only make reads which move nothing, and have made no call when the others start.
const OBSERVERS: usize = 3;
/// Threads reading as each observer at once, so its first call is raced with itself.
const THREADS_PER_OBSERVER: usize = 2;
const OBSERVER_ROUNDS: usize = 25;
/// Threads that have expired when the others start. Their messages alone are more than the one
/// small sweep the first change runs can remove, so the sweeping thread has every thread row
/// left to remove while the others run.
const EXPIRED_THREADS: usize = 40;

/// Records one concurrent sweep removes at most, small enough that sweeping takes many rounds.
const SWEEP_STEP: usize = 5;

/// Clears the flag when dropped, so the thread that moves the clock ends when the readers are
/// joined and also when one of them panicked.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Every message row of the store by thread, as `(seq, row)` in key order.
fn stored_messages(store: &FjallAgents) -> BTreeMap<ThreadId, Vec<(u64, MessageMeta)>> {
    let mut stored: BTreeMap<ThreadId, Vec<(u64, MessageMeta)>> = BTreeMap::new();
    for guard in store.ks.messages.iter() {
        let (key, value) = guard.into_inner().expect("message row");
        let thread = keys::parse_thread(&key[..4]).expect("thread of a message key");
        let seq = keys::parse_message_seq(&key).expect("seq of a message key");
        let meta = records::decode::<MessageMeta>("a message", &value).expect("message");
        stored.entry(thread).or_default().push((seq, meta));
    }
    stored
}

/// The sequence numbers each thread delivered over `replies`, in the order they came.
fn delivered(replies: &[InboxReply]) -> BTreeMap<ThreadId, Vec<u64>> {
    let mut delivered: BTreeMap<ThreadId, Vec<u64>> = BTreeMap::new();
    for message in replies.iter().flat_map(|reply| &reply.messages) {
        delivered
            .entry(message.id.thread().clone())
            .or_default()
            .push(message.id.seq());
    }
    delivered
}

/// Every reply of a read that marks must report, as read, exactly the last message it returned
/// in each thread.
fn assert_read_to_is_the_last_returned(name: &str, replies: &[InboxReply]) {
    for reply in replies {
        let last: BTreeSet<MessageId> = delivered(std::slice::from_ref(reply))
            .into_iter()
            .map(|(thread, seqs)| MessageId::new(thread, *seqs.last().expect("a delivered seq")).expect("id"))
            .collect();
        let read_to: BTreeSet<MessageId> = reply.read_to.iter().cloned().collect();
        assert_eq!(
            read_to, last,
            "{name} was told it read something else than it was given"
        );
    }
}

/// What a reader was given in each thread must be every message of somebody else up to the last
/// one it was given, each once and in order: a read position never passed a message that was
/// not returned.
fn assert_nothing_was_stepped_over(
    name: &str,
    replies: &[InboxReply],
    stored: &BTreeMap<ThreadId, Vec<(u64, MessageMeta)>>,
) {
    let reader = caller(name).actor;
    for (thread, seqs) in delivered(replies) {
        let last = *seqs.last().expect("a delivered seq");
        let expected: Vec<u64> = stored[&thread]
            .iter()
            .filter(|(seq, meta)| *seq <= last && meta.from != reader)
            .map(|(seq, _)| *seq)
            .collect();
        assert_eq!(seqs, expected, "{name} in {thread}");
    }
}

/// Reads that move nothing, as `name`, while the others write: a peek, the history of `shared`,
/// its description and a listing. Each runs on a snapshot taken without the writer lock, so each
/// must see `shared` whole as of one instant, and never older than the read before it.
fn observe(harness: &Harness, name: &str, shared: &ThreadId) {
    let observer = caller(name);
    let seqs_of = |reply: &InboxReply| -> Vec<u64> {
        assert!(reply.read_to.is_empty(), "{name} moved a position without marking");
        reply
            .messages
            .iter()
            .inspect(|message| assert_eq!(message.id.thread(), shared, "{name} is in no other thread"))
            .map(|message| message.id.seq())
            .collect()
    };
    let mut last_head = 0;
    for _ in 0..OBSERVER_ROUNDS {
        let peeked = seqs_of(&harness.inbox(&observer, peek()).expect("peek"));
        assert_eq!(
            peeked,
            (1..=peeked.len() as u64).collect::<Vec<u64>>(),
            "{name} peeked a gap"
        );
        let whole = InboxBody {
            select: InboxSelect::Thread(shared.clone()),
            limit: Some(100),
            ..history()
        };
        let read = seqs_of(&harness.inbox(&observer, whole).expect("history"));
        assert_eq!(read, (1..=read.len() as u64).collect::<Vec<u64>>(), "{name} read a gap");
        assert!(read.len() as u64 >= last_head, "{name} read an older store");
        let described = harness
            .threads_query(&observer, ThreadQuery::Describe(shared.clone()))
            .expect("describe");
        let head = described.threads[0].head;
        assert!(head >= read.len() as u64, "{name} was described an older thread");
        assert_eq!(
            described.threads[0].unread, head,
            "{name} read nothing and posted nothing"
        );
        let listed = harness.threads_query(&observer, ThreadQuery::default()).expect("list");
        assert_eq!(listed.threads.len(), 1, "{name} is in one thread");
        assert!(listed.threads[0].head >= head, "{name} was listed an older thread");
        last_head = listed.threads[0].head;
    }
}

#[test]
fn concurrent_posts_reads_creations_leaves_and_sweeps_lose_and_skip_nothing() {
    let harness = Harness::new();
    for n in 0..EXPIRED_THREADS {
        let old = start(&harness, "old", &format!("old {n}"), &["p0"]);
        say(&harness, "old", &old.thread, "second");
    }
    harness.clock.advance(MESSAGE_TTL + Duration::from_secs(1));
    let posters: Vec<String> = (0..POSTERS).map(|n| format!("p{n}")).collect();
    let leavers: Vec<String> = (0..LEAVERS).map(|n| format!("l{n}")).collect();
    let observers: Vec<String> = (0..OBSERVERS).map(|n| format!("o{n}")).collect();
    let everyone: Vec<&str> = posters
        .iter()
        .chain(&leavers)
        .chain(&observers)
        .map(String::as_str)
        .collect();
    let shared = start(&harness, "a", "shared", &everyone);
    for name in &observers {
        assert_eq!(harness.store().recorded_seen(name), None, "{name} has made no call yet");
    }
    let other = start(&harness, "a", "other", &everyone[..POSTERS]);
    let before = harness.arrivals();
    let running = AtomicBool::new(true);

    let mut reads: Vec<(&str, Vec<InboxReply>)> = std::thread::scope(|scope| {
        let (harness, shared, other, running) = (&harness, &shared.thread, &other.thread, &running);
        // ~keep The clock moves while the others run, so two writes of one record made at two
        // instants leave two index entries unless the second replaces the first.
        scope.spawn(move || {
            while running.load(Ordering::Acquire) {
                harness.clock.advance(Duration::from_micros(1));
                std::thread::yield_now();
            }
        });
        let marking_read = move |name: &str| harness.inbox(&caller(name), InboxBody::default()).expect("inbox");
        scope.spawn(move || {
            let mut threads = 0;
            loop {
                let report = harness.store().sweep_bounded(SWEEP_STEP).expect("sweep");
                threads += report.threads;
                if !report.more {
                    break;
                }
            }
            assert_eq!(threads, EXPIRED_THREADS, "the sweep ran beside the other threads");
        });
        for name in observers
            .iter()
            .flat_map(|name| std::iter::repeat_n(name, THREADS_PER_OBSERVER))
        {
            scope.spawn(move || observe(harness, name, shared));
        }
        for creator in 0..CREATORS {
            scope.spawn(move || {
                for _ in 0..THREADS_EACH {
                    start(harness, &format!("c{creator}"), "made meanwhile", &["p0", "l0"]);
                }
            });
        }
        let readers: Vec<_> = posters
            .iter()
            .map(|name| {
                scope.spawn(move || {
                    let mut replies = Vec::new();
                    for n in 0..POSTS_EACH {
                        let thread = if n % 2 == 0 { shared } else { other };
                        harness
                            .post(&caller(name), into(thread, &format!("{name} {n}")))
                            .expect("post");
                        replies.push(marking_read(name));
                    }
                    (name.as_str(), replies)
                })
            })
            .chain(leavers.iter().map(|name| {
                scope.spawn(move || {
                    let replies = (0..READS_BEFORE_LEAVING).map(|_| marking_read(name)).collect();
                    let left = ThreadUpdate {
                        thread: shared.clone(),
                        action: ThreadAction::Leave,
                    };
                    harness.threads_update(&caller(name), left).expect("leave");
                    (name.as_str(), replies)
                })
            }))
            .collect();
        let _stop_the_clock = StopOnDrop(running);
        readers
            .into_iter()
            .map(|reader| reader.join().expect("a reader panicked"))
            .collect()
    });

    let posted = POSTERS * POSTS_EACH;
    let created = CREATORS * THREADS_EACH;
    assert_eq!(harness.arrivals(), before + (posted + created) as u64);
    for (name, replies) in reads.iter_mut().take(POSTERS) {
        loop {
            let reply = harness.inbox(&caller(name), InboxBody::default()).expect("inbox");
            if reply.messages.is_empty() {
                break;
            }
            replies.push(reply);
        }
    }
    let store = harness.store();
    while store.sweep().expect("sweep").more {}
    let stored = stored_messages(store);

    assert_eq!(stored.len(), 2 + created, "the expired threads are gone and no other");
    let reader = store.reader(store.clock.now_micros());
    for (thread, messages) in &stored {
        let head = reader.thread(thread).expect("read").expect("thread").head;
        let seqs: Vec<u64> = messages.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, (1..=head).collect::<Vec<u64>>(), "{thread} is numbered densely");
    }
    assert_eq!(stored[&shared.thread].len() + stored[&other.thread].len(), 2 + posted);
    let mut in_post_order: Vec<&MessageMeta> = stored.values().flatten().map(|(_, meta)| meta).collect();
    in_post_order.sort_by_key(|meta| meta.order);
    assert!(
        in_post_order
            .windows(2)
            .all(|pair| pair[0].order < pair[1].order && pair[0].ts_micros < pair[1].ts_micros),
        "post positions are unique and timestamps follow them"
    );
    assert_eq!(in_post_order.len(), 2 + posted + created);
    assert_eq!(in_post_order.last().map(|meta| meta.order), Some(harness.arrivals()));

    for (name, replies) in &reads {
        assert_read_to_is_the_last_returned(name, replies);
        assert_nothing_was_stepped_over(name, replies, &stored);
    }
    for (name, replies) in reads.iter().take(POSTERS) {
        let actor = caller(name).actor;
        let given = delivered(replies);
        for (thread, messages) in &stored {
            let Some(position) = reader.position(thread, &actor).expect("member row") else {
                continue;
            };
            let from_others: Vec<u64> = messages
                .iter()
                .filter(|(_, meta)| meta.from != actor)
                .map(|(seq, _)| *seq)
                .collect();
            assert_eq!(given.get(thread), Some(&from_others), "{name} read all of {thread}");
            assert_eq!(Some(&position.read_seq), from_others.last(), "{name} in {thread}");
        }
    }
    for name in &leavers {
        assert_eq!(harness.position(&shared.thread, &caller(name).actor), None);
    }
    assert!(!reader.thread(&shared.thread).expect("read").expect("thread").archived);

    let rows = store.stored_rows();
    let callers = ["old", "a"].len() + POSTERS + LEAVERS + CREATORS + OBSERVERS;
    assert_eq!(rows[8], callers, "one seen record for each agent that made a call");
    assert_eq!(rows[0], stored.len(), "a thread row for each thread with messages");
    assert_eq!(
        rows[9],
        rows[0] + rows[8] + rows[7],
        "one expiry entry for each thread, seen record and replay entry"
    );
}
