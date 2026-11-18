//! The hub's wait, its replay join and its notices, over a real store in a temporary directory.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::*;
use crate::comms::v2::api::{InboxSelect, PostTarget, ReplayKey, ThreadId};
use crate::comms::v2::conformance::{Repo, default_caller};

fn hub() -> (Arc<AgentHub>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temporary directory");
    (Arc::new(AgentHub::open(dir.path())), dir)
}

/// A client that is still there.
fn live() -> CancellationToken {
    CancellationToken::new()
}

fn caller(name: &str) -> Caller {
    default_caller(name, Repo::Home)
}

fn new_thread(to: &[&str]) -> PostBody {
    PostBody {
        target: PostTarget::NewThread {
            subject: "topic".to_string(),
            to: to.iter().map(|name| caller(name).actor).collect(),
            paths: Vec::new(),
        },
        body: "first".to_string(),
    }
}

fn into(thread: &ThreadId, body: &str) -> PostBody {
    PostBody {
        target: PostTarget::Thread(thread.clone()),
        body: body.to_string(),
    }
}

fn only(thread: &ThreadId) -> InboxBody {
    InboxBody {
        select: InboxSelect::Thread(thread.clone()),
        ..InboxBody::default()
    }
}

#[tokio::test]
async fn a_wait_ends_when_another_agent_posts_and_marks_what_it_returns() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    hub.read(
        caller("b"),
        InboxBody::default(),
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .await
    .expect("b reads the first post");

    let waiting = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.read(
                caller("b"),
                InboxBody::default(),
                Duration::from_secs(20),
                &CancellationToken::new(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.post(caller("a"), into(&thread, "news"), &live())
        .await
        .expect("post");

    let reply = waiting.await.expect("waiter").expect("read");
    assert_eq!(reply.wait, WaitOutcome::Arrived);
    assert_eq!(reply.messages.len(), 1);
    assert_eq!(reply.messages[0].body, "news");
    let again = hub
        .read(
            caller("b"),
            InboxBody::default(),
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .expect("read");
    assert!(again.messages.is_empty(), "the waited read marked what it returned");
}

#[tokio::test]
async fn the_callers_own_post_does_not_end_its_wait() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    hub.read(caller("a"), only(&thread), Duration::ZERO, &CancellationToken::new())
        .await
        .expect("a reads");

    let waiting = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.read(
                caller("a"),
                InboxBody::default(),
                Duration::from_millis(400),
                &CancellationToken::new(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.post(caller("a"), into(&thread, "mine"), &live())
        .await
        .expect("post");

    let reply = waiting.await.expect("waiter").expect("read");
    assert_eq!(reply.wait, WaitOutcome::TimedOut);
    assert!(reply.messages.is_empty());
}

#[tokio::test]
async fn a_wait_on_one_thread_ignores_posts_to_another() {
    let (hub, _dir) = hub();
    let watched = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    let other = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    hub.read(
        caller("b"),
        InboxBody::default(),
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .await
    .expect("b reads both first posts");

    let waiting = {
        let (hub, watched) = (Arc::clone(&hub), watched.clone());
        tokio::spawn(async move {
            hub.read(
                caller("b"),
                only(&watched),
                Duration::from_secs(20),
                &CancellationToken::new(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.post(caller("a"), into(&other, "elsewhere"), &live())
        .await
        .expect("post");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !waiting.is_finished(),
        "a post to another thread ends no wait on this one"
    );
    hub.post(caller("a"), into(&watched, "here"), &live())
        .await
        .expect("post");

    let reply = waiting.await.expect("waiter").expect("read");
    assert_eq!(reply.messages.len(), 1);
    assert_eq!(reply.messages[0].body, "here");
    let rest = hub
        .read(
            caller("b"),
            InboxBody::default(),
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .expect("read");
    assert_eq!(rest.messages.len(), 1, "the other thread's post is still unread");
    assert_eq!(rest.messages[0].body, "elsewhere");
}

#[tokio::test]
async fn a_waiter_whose_client_has_gone_marks_nothing_read() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    hub.read(
        caller("b"),
        InboxBody::default(),
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .await
    .expect("b reads");
    let gone = CancellationToken::new();
    gone.cancel();
    hub.post(caller("a"), into(&thread, "unseen"), &live())
        .await
        .expect("post");

    let reply = hub
        .read(caller("b"), InboxBody::default(), Duration::from_secs(5), &gone)
        .await
        .expect("read");
    assert!(reply.messages.is_empty());
    let after = hub
        .read(
            caller("b"),
            InboxBody::default(),
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .expect("read");
    assert_eq!(after.messages.len(), 1, "the message stayed unread");
}

#[tokio::test]
async fn a_replay_that_arrives_during_a_wait_gets_the_same_reply() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    hub.read(
        caller("b"),
        InboxBody::default(),
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .await
    .expect("b reads");
    let keyed = caller("b").with_replay_key(ReplayKey::parse("relay-1").expect("key"));
    let read = |hub: Arc<AgentHub>, keyed: Caller| {
        tokio::spawn(async move {
            hub.read(
                keyed,
                InboxBody::default(),
                Duration::from_secs(20),
                &CancellationToken::new(),
            )
            .await
        })
    };
    let first = read(Arc::clone(&hub), keyed.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let replay = read(Arc::clone(&hub), keyed);
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.post(caller("a"), into(&thread, "once"), &live())
        .await
        .expect("post");

    let first = first.await.expect("first").expect("read");
    let replay = replay.await.expect("replay").expect("read");
    assert_eq!(first.messages, replay.messages);
    assert_eq!(first.messages.len(), 1);
}

#[tokio::test]
async fn two_sessions_of_one_principal_each_read_every_message() {
    let (hub, _dir) = hub();
    let session = |name: &str| {
        let principal = caller("a").actor.principal_id();
        let sub = crate::comms::v2::api::SubName::parse(name).expect("sub-name");
        Caller::new(Actor::new(&principal, Some(&sub)).expect("actor"), caller("a").repo)
    };
    let (one, two) = (session("session-11111111"), session("session-22222222"));
    let thread = hub
        .post(caller("b"), new_thread(&["a"]), &live())
        .await
        .expect("start")
        .thread;
    for (reader, expected) in [(&one, 1), (&two, 0)] {
        let first = hub
            .read(
                reader.clone(),
                InboxBody::default(),
                Duration::ZERO,
                &CancellationToken::new(),
            )
            .await
            .expect("read");
        assert_eq!(
            first.messages.len(),
            expected,
            "a session's first read starts where a sibling has read (ADR-0021 rule 5)"
        );
    }
    hub.post(caller("b"), into(&thread, "for both"), &live())
        .await
        .expect("post");

    for reader in [&one, &two] {
        let reply = hub
            .read(
                reader.clone(),
                InboxBody::default(),
                Duration::ZERO,
                &CancellationToken::new(),
            )
            .await
            .expect("read");
        assert_eq!(
            reply.messages.len(),
            1,
            "one session's read does not consume the other's mail"
        );
        assert_eq!(reply.messages[0].body, "for both");
    }
}

#[tokio::test]
async fn sessions_sharing_a_principal_are_told_unless_they_pass_as() {
    let (hub, _dir) = hub();
    let principal = caller("a").actor.principal_id();
    let _one = hub.serve_session(principal.clone());
    assert!(hub.notices_for(&caller("a").actor).is_empty());
    let two = hub.serve_session(principal);
    let reply = hub.post(caller("a"), new_thread(&["b"]), &live()).await.expect("post");
    assert_eq!(reply.notices.len(), 1, "{:?}", reply.notices);
    drop(two);
    assert!(hub.notices_for(&caller("a").actor).is_empty());
}

#[tokio::test]
async fn cleanup_archives_a_thread_its_caller_did_not_create() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    let report = hub.cleanup(vec![thread.clone()], &live()).await.expect("cleanup");
    assert_eq!(report.archived, std::slice::from_ref(&thread));
    let again = hub.cleanup(vec![thread.clone()], &live()).await.expect("cleanup");
    assert_eq!(again.already_archived, std::slice::from_ref(&thread));
    let refused = hub.post(caller("a"), into(&thread, "late"), &live()).await;
    assert!(matches!(refused, Err(CommsError::ThreadArchived { .. })), "{refused:?}");
}

#[test]
fn an_unreadable_store_is_reported_and_refuses_agent_calls() {
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::write(dir.path().join(STORE_DIR), b"not a directory").expect("block the store");
    let hub = AgentHub::open(dir.path());
    tokio::runtime::Runtime::new().expect("runtime").block_on(hub.opened());
    assert!(matches!(
        hub.ops().post(&caller("a"), new_thread(&["b"])),
        Err(CommsError::Backend { .. })
    ));
    let status = hub.status(None, None);
    assert_eq!(status.state, "unreadable");
    assert!(status.error.is_some());
}

/// A relayed marking read that waited, delivered and finished, then replayed with a wait on the
/// hub `reopen` returns.
async fn keyed_wait_then_replay(first: Arc<AgentHub>, reopen: impl FnOnce(Arc<AgentHub>) -> Arc<AgentHub>) {
    let thread = first
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    first
        .read(
            caller("b"),
            InboxBody::default(),
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .expect("b reads the first post");
    first
        .post(caller("a"), into(&thread, "once"), &live())
        .await
        .expect("post");
    let keyed = caller("b").with_replay_key(ReplayKey::parse("relay-after").expect("key"));
    let delivered = first
        .read(
            keyed.clone(),
            InboxBody::default(),
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .expect("read");
    assert_eq!(delivered.messages.len(), 1);

    let hub = reopen(first);
    let replayed = tokio::time::timeout(
        Duration::from_secs(3),
        hub.read(
            keyed,
            InboxBody::default(),
            Duration::from_secs(30),
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("a replay of a read that delivered answers at once")
    .expect("read");
    assert_eq!(
        replayed.messages, delivered.messages,
        "the replay returns what the first delivered"
    );
}

#[tokio::test]
async fn a_replayed_waiting_read_that_finished_returns_what_it_delivered() {
    let (hub, _dir) = hub();
    keyed_wait_then_replay(hub, |same| same).await;
}

#[tokio::test]
async fn a_replayed_waiting_read_on_a_restarted_daemon_returns_what_it_delivered() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().to_path_buf();
    let first = Arc::new(AgentHub::open(&path));
    keyed_wait_then_replay(first, move |first| {
        drop(first);
        Arc::new(AgentHub::open(&path))
    })
    .await;
}

#[tokio::test]
async fn a_replay_that_joined_a_waiter_whose_client_left_keeps_waiting() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    hub.read(
        caller("b"),
        InboxBody::default(),
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .await
    .expect("b reads");
    let keyed = caller("b").with_replay_key(ReplayKey::parse("relay-gone").expect("key"));
    let left = CancellationToken::new();
    let first = {
        let (hub, keyed, left) = (Arc::clone(&hub), keyed.clone(), left.clone());
        tokio::spawn(async move {
            hub.read(keyed, InboxBody::default(), Duration::from_secs(20), &left)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let replay = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.read(
                keyed,
                InboxBody::default(),
                Duration::from_secs(20),
                &CancellationToken::new(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    left.cancel();
    let first = first.await.expect("first").expect("read");
    assert!(
        first.messages.is_empty(),
        "the first's client left before anything arrived"
    );
    hub.post(caller("a"), into(&thread, "later"), &live())
        .await
        .expect("post");

    let replay = replay.await.expect("replay").expect("read");
    assert_eq!(replay.messages.len(), 1, "the replay waited on after the first left");
    assert_eq!(replay.messages[0].body, "later");
}

#[tokio::test]
async fn a_waiting_peek_behind_the_callers_own_posts_returns_its_cursor() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("b"), new_thread(&["a"]), &live())
        .await
        .expect("start")
        .thread;
    // Past the store's scan budget of 4 * MAX_LIMIT rows a read walks beyond each thread's first.
    let own = 4 * crate::comms::v2::api::MAX_LIMIT as usize + 8;
    {
        let hub = Arc::clone(&hub);
        let thread = thread.clone();
        tokio::task::spawn_blocking(move || {
            for n in 0..own {
                hub.ops()
                    .post(&caller("b"), into(&thread, &format!("own {n}")))
                    .expect("own post");
            }
        })
        .await
        .expect("posts");
    }
    hub.post(caller("a"), into(&thread, "reply"), &live())
        .await
        .expect("reply");
    let peek = InboxBody {
        mark: Mark::Peek,
        ..InboxBody::default()
    };

    let mut cursor = None;
    let mut seen = Vec::new();
    for _ in 0..8 {
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            hub.read(
                caller("b"),
                InboxBody {
                    cursor: cursor.take(),
                    ..peek.clone()
                },
                Duration::from_secs(30),
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("a peek that stopped at its budget answers at once")
        .expect("peek");
        seen.extend(reply.messages.iter().map(|message| message.body.clone()));
        if !seen.is_empty() {
            break;
        }
        assert!(
            reply.more && reply.next_cursor.is_some(),
            "an empty stop says where to go on"
        );
        cursor = reply.next_cursor;
    }
    assert_eq!(seen, ["reply"]);
}

#[tokio::test]
async fn a_drain_ends_every_wait() {
    let (hub, _dir) = hub();
    hub.post(caller("a"), new_thread(&["b"]), &live()).await.expect("start");
    hub.read(
        caller("b"),
        InboxBody::default(),
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .await
    .expect("b reads");
    let waiting = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.read(
                caller("b"),
                InboxBody::default(),
                Duration::from_secs(40),
                &CancellationToken::new(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.shut_down();
    let reply = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("the drain ended the wait")
        .expect("waiter")
        .expect("read");
    assert_eq!(reply.wait, WaitOutcome::TimedOut);
}

#[tokio::test]
async fn a_store_another_process_holds_opens_once_it_is_released() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let holder = FjallAgents::open(dir.path()).expect("the first opener holds the store");
    let hub = Arc::new(AgentHub::open(dir.path()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        hub.status(None, None).state,
        "opening",
        "a held store is waited for, not refused"
    );
    drop(holder);
    tokio::time::timeout(Duration::from_secs(10), hub.opened())
        .await
        .expect("the store opens once released");
    assert_eq!(hub.status(None, None).state, "open");
    hub.post(caller("a"), new_thread(&["b"]), &live()).await.expect("post");
}

#[tokio::test]
async fn a_replay_of_a_read_whose_first_request_was_dropped_waits_on_its_own() {
    let (hub, _dir) = hub();
    let thread = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("start")
        .thread;
    hub.read(caller("b"), InboxBody::default(), Duration::ZERO, &live())
        .await
        .expect("b reads");
    let keyed = caller("b").with_replay_key(ReplayKey::parse("relay-dropped").expect("key"));
    let client = live();
    let first = hub.read(keyed.clone(), InboxBody::default(), Duration::from_secs(30), &client);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), first).await.is_err(),
        "the first read was still waiting when it was dropped"
    );
    let replay = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.read(keyed, InboxBody::default(), Duration::from_secs(30), &live())
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    hub.post(caller("a"), into(&thread, "after"), &live())
        .await
        .expect("post");

    let replay = tokio::time::timeout(Duration::from_secs(5), replay)
        .await
        .expect("the replay is answered by the post, not at its deadline")
        .expect("replay")
        .expect("read");
    assert_eq!(replay.messages.len(), 1);
    assert_eq!(replay.messages[0].body, "after");
}

#[tokio::test]
async fn a_post_whose_client_left_while_the_store_opened_writes_nothing() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let hub = Arc::new(AgentHub::open_after(dir.path(), Duration::from_millis(300)));
    let gone = CancellationToken::new();
    let post = {
        let (hub, gone) = (Arc::clone(&hub), gone.clone());
        tokio::spawn(async move { hub.post(caller("a"), new_thread(&["b"]), &gone).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    gone.cancel();
    assert!(post.await.expect("post task").is_err(), "the post is refused");
    let threads = hub
        .threads_query(caller("a"), ThreadQuery::default())
        .await
        .expect("threads");
    assert!(threads.threads.is_empty(), "nothing was written: {threads:?}");
}

#[tokio::test(start_paused = true)]
async fn an_agent_request_waits_for_the_open_less_than_its_client_does() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let hub = Arc::new(AgentHub::open_after(dir.path(), Duration::from_secs(5)));
    let refused = hub
        .post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect_err("still opening after the wait");
    assert!(refused.to_string().contains("still opening"), "{refused}");
}

#[tokio::test]
async fn a_store_held_past_the_opens_patience_is_tried_again_on_the_next_request() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let holder = FjallAgents::open(dir.path()).expect("the first opener holds the store");
    let hub = Arc::new(AgentHub::open_patiently(
        dir.path(),
        Duration::ZERO,
        Duration::from_millis(200),
    ));
    tokio::time::timeout(Duration::from_secs(10), hub.opened())
        .await
        .expect("the open gave up");
    assert_eq!(hub.status(None, None).state, "unreadable");
    drop(holder);
    hub.post(caller("a"), new_thread(&["b"]), &live())
        .await
        .expect("the next request opens the released store");
    assert_eq!(hub.status(None, None).state, "open");
}

/// Each `Hello` names a root off the wire; the repository cache holds at most its cap of them.
#[test]
fn the_repository_cache_is_bounded() {
    let (hub, dir) = hub();
    for n in 0..=REPO_CACHE_CAP {
        hub.repo_for(&dir.path().join(format!("root-{n}"))).expect("repository");
    }
    assert!(lock(&hub.repos).len() <= REPO_CACHE_CAP);
}
