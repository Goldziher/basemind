//! What is true of the fake only: its deterministic ids and clock, and the exact shape of what
//! it returns. The rules themselves are the conformance suite, instantiated below.

use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::comms::v2::api::WaitOutcome;
use crate::comms::v2::conformance::{Repo, default_caller, wait_then_read};

mod conformance {
    use crate::comms::v2::fake::FakeAgents;

    crate::conformance_tests!(FakeAgents::new());
}

fn caller(name: &str) -> Caller {
    default_caller(name, Repo::Home)
}

fn thread(s: &str) -> ThreadId {
    ThreadId::parse(s).expect("thread id")
}

fn message(s: &str) -> MessageId {
    MessageId::parse(s).expect("message id")
}

fn start(fake: &FakeAgents, creator: &str, subject: &str, to: &[&str]) -> PostReply {
    let body = PostBody {
        target: PostTarget::NewThread {
            subject: subject.to_string(),
            to: to.iter().map(|name| caller(name).actor).collect(),
            paths: Vec::new(),
        },
        body: "first".to_string(),
    };
    fake.post(&caller(creator), body).expect("thread created")
}

#[test]
fn thread_ids_are_drawn_from_a_counter() {
    let fake = FakeAgents::new();

    assert_eq!(
        start(&fake, "a", "topic", &["b"]),
        PostReply {
            id: message("t-aaaaaa.1"),
            thread: thread("t-aaaaaa"),
            created: true,
            pending: vec![caller("b").actor],
            notices: Vec::new(),
        }
    );
    assert_eq!(start(&fake, "a", "topic", &[]).thread, thread("t-aaaaab"));
}

#[test]
fn colliding_thread_id_is_drawn_again() {
    let fake = FakeAgents::new();
    fake.queue_thread_bits([5, 5, 6]);

    assert_eq!(start(&fake, "a", "one", &[]).thread, thread("t-aaaaaf"));
    assert_eq!(start(&fake, "a", "two", &[]).thread, thread("t-aaaaag"));
}

#[test]
fn clock_starts_at_the_fake_epoch_and_each_post_takes_a_microsecond() {
    let fake = FakeAgents::new();
    start(&fake, "a", "topic", &["b"]);

    assert_eq!(
        fake.inbox(&caller("b"), InboxBody::default()).expect("inbox"),
        InboxReply {
            messages: vec![InboxMessage {
                id: message("t-aaaaaa.1"),
                subject: "topic".to_string(),
                from: caller("a").actor,
                ts_micros: FAKE_EPOCH_MICROS + 1,
                reply_to: None,
                body: "first".to_string(),
                truncated: false,
            }],
            read_to: vec![message("t-aaaaaa.1")],
            more: false,
            next_cursor: None,
            notices: Vec::new(),
            wait: WaitOutcome::NotWaited,
        }
    );
    assert_eq!(
        fake.threads_query(&caller("a"), ThreadQuery::default())
            .expect("threads"),
        ThreadsReply {
            threads: vec![ThreadSummary {
                id: thread("t-aaaaaa"),
                subject: "topic".to_string(),
                repo: caller("a").repo,
                paths: Vec::new(),
                open: false,
                archived: false,
                creator: caller("a").actor,
                member: true,
                unread: 0,
                head: 1,
                last_activity_micros: FAKE_EPOCH_MICROS + 1,
                peers: vec![caller("b").actor],
            }],
            pending: Vec::new(),
            next_cursor: None,
            notices: Vec::new(),
        }
    );
}

#[test]
fn one_instance_serves_a_waiter_and_a_poster_on_different_threads() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    fake.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    let ops: &dyn AgentOps = &fake;
    let (waiting, poster_may_post) = mpsc::channel();

    let reply = std::thread::scope(|scope| {
        scope.spawn(move || {
            poster_may_post
                .recv_timeout(Duration::from_secs(30))
                .expect("the waiter started waiting");
            let body = PostBody {
                target: PostTarget::Thread(created.thread),
                body: "ping".to_string(),
            };
            ops.post(&caller("a"), body).expect("post");
        });
        wait_then_read(ops, &caller("b"), InboxBody::default(), Duration::from_secs(30), || {
            waiting.send(()).expect("the poster is listening");
        })
        .expect("inbox")
    });

    assert_eq!(reply.wait, WaitOutcome::Arrived);
    assert_eq!(reply.messages.len(), 1);
    assert_eq!(reply.messages[0].body, "ping");
}
