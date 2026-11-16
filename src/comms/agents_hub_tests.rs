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
    let thread = hub.post(caller("a"), new_thread(&["b"])).await.expect("start").thread;
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
    hub.post(caller("a"), into(&thread, "news")).await.expect("post");

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
    let thread = hub.post(caller("a"), new_thread(&["b"])).await.expect("start").thread;
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
    hub.post(caller("a"), into(&thread, "mine")).await.expect("post");

    let reply = waiting.await.expect("waiter").expect("read");
    assert_eq!(reply.wait, WaitOutcome::TimedOut);
    assert!(reply.messages.is_empty());
}

#[tokio::test]
async fn a_wait_on_one_thread_ignores_posts_to_another() {
    let (hub, _dir) = hub();
    let watched = hub.post(caller("a"), new_thread(&["b"])).await.expect("start").thread;
    let other = hub.post(caller("a"), new_thread(&["b"])).await.expect("start").thread;
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
    hub.post(caller("a"), into(&other, "elsewhere")).await.expect("post");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !waiting.is_finished(),
        "a post to another thread ends no wait on this one"
    );
    hub.post(caller("a"), into(&watched, "here")).await.expect("post");

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
    let thread = hub.post(caller("a"), new_thread(&["b"])).await.expect("start").thread;
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
    hub.post(caller("a"), into(&thread, "unseen")).await.expect("post");

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
    let thread = hub.post(caller("a"), new_thread(&["b"])).await.expect("start").thread;
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
    hub.post(caller("a"), into(&thread, "once")).await.expect("post");

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
    let thread = hub.post(caller("b"), new_thread(&["a"])).await.expect("start").thread;
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
    hub.post(caller("b"), into(&thread, "for both")).await.expect("post");

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
    let reply = hub.post(caller("a"), new_thread(&["b"])).await.expect("post");
    assert_eq!(reply.notices.len(), 1, "{:?}", reply.notices);
    drop(two);
    assert!(hub.notices_for(&caller("a").actor).is_empty());
}

#[tokio::test]
async fn cleanup_archives_a_thread_its_caller_did_not_create() {
    let (hub, _dir) = hub();
    let thread = hub.post(caller("a"), new_thread(&["b"])).await.expect("start").thread;
    let report = hub.cleanup(vec![thread.clone()]).await.expect("cleanup");
    assert_eq!(report.archived, std::slice::from_ref(&thread));
    let again = hub.cleanup(vec![thread.clone()]).await.expect("cleanup");
    assert_eq!(again.already_archived, std::slice::from_ref(&thread));
    let refused = hub.post(caller("a"), into(&thread, "late")).await;
    assert!(matches!(refused, Err(CommsError::ThreadArchived { .. })), "{refused:?}");
}

#[test]
fn an_unreadable_store_is_reported_and_refuses_agent_calls() {
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::write(dir.path().join(STORE_DIR), b"not a directory").expect("block the store");
    let hub = AgentHub::open(dir.path());
    assert!(matches!(
        hub.ops().post(&caller("a"), new_thread(&["b"])),
        Err(CommsError::Backend { .. })
    ));
    let status = hub.status(None, None);
    assert_eq!(status.state, "unreadable");
    assert!(status.error.is_some());
}
