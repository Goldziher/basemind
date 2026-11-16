//! Unit tests for the comms [`Broker`](super::Broker). Split out of `daemon.rs` (via a
//! `#[cfg(test)] #[path = "daemon_tests.rs"] mod tests;` declaration) to keep `daemon.rs` under
//! the 1000-line `rust-max-lines` cap. `super` here resolves to the `daemon` module.

use super::*;
use crate::comms::v2::api::{InboxBody, PostBody, PostTarget, Principal, SubName, ThreadQuery};

#[path = "daemon_lifecycle_tests.rs"]
mod lifecycle_tests;
#[path = "daemon_maintenance_tests.rs"]
mod maintenance_tests;

fn temp_broker() -> (tempfile::TempDir, Arc<Broker>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let agents = super::super::agents_hub::AgentHub::open(dir.path());
    (dir, Arc::new(Broker::new(agents)))
}

/// A link that said hello as `principal` from `root`.
async fn hello(broker: &Broker, principal: &str, root: &std::path::Path) -> Session {
    let mut session = Session::default();
    let welcome = broker
        .handle(
            CommsRequest::Hello {
                proto_ver: PROTO_VER,
                principal: Some(Principal::parse(principal).expect("principal")),
                root: Some(root.to_path_buf()),
                host: None,
            },
            &mut session,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(welcome, CommsResponse::Welcome { .. }), "{welcome:?}");
    session
}

fn new_thread(to: &[&str]) -> PostBody {
    PostBody {
        target: PostTarget::NewThread {
            subject: "topic".to_string(),
            to: to
                .iter()
                .map(|name| crate::comms::v2::api::Actor::parse(*name).expect("actor"))
                .collect(),
            paths: Vec::new(),
        },
        body: "hello".to_string(),
    }
}

#[tokio::test]
async fn hello_rejects_proto_skew() {
    let (_dir, broker) = temp_broker();
    let mut session = Session::default();
    let response = broker
        .handle(
            CommsRequest::Hello {
                proto_ver: PROTO_VER + 1,
                principal: None,
                root: None,
                host: None,
            },
            &mut session,
            &CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(response, CommsResponse::Error { ref code, .. } if code == "proto_skew"),
        "{response:?}"
    );
}

#[tokio::test]
async fn an_agent_request_on_a_link_without_an_identity_is_refused() {
    let (_dir, broker) = temp_broker();
    let mut session = Session::default();
    let response = broker
        .handle(
            CommsRequest::AgentPost {
                sub: None,
                body: new_thread(&["bob"]),
            },
            &mut session,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(response, CommsResponse::AgentRefused(_)), "{response:?}");
}

#[tokio::test]
async fn a_post_on_one_link_reaches_the_inbox_of_another() {
    let (dir, broker) = temp_broker();
    let mut alice = hello(&broker, "alice", dir.path()).await;
    let mut bob = hello(&broker, "bob", dir.path()).await;
    let posted = broker
        .handle(
            CommsRequest::AgentPost {
                sub: None,
                body: new_thread(&["bob"]),
            },
            &mut alice,
            &CancellationToken::new(),
        )
        .await;
    let CommsResponse::AgentPosted(reply) = posted else {
        panic!("not posted: {posted:?}");
    };
    assert!(reply.created);

    let read = broker
        .handle(
            CommsRequest::AgentInbox {
                sub: None,
                body: InboxBody::default(),
                wait_secs: 0,
            },
            &mut bob,
            &CancellationToken::new(),
        )
        .await;
    let CommsResponse::AgentRead(inbox) = read else {
        panic!("not read: {read:?}");
    };
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].from.as_str(), "alice");
    assert_eq!(inbox.messages[0].body, "hello");
}

#[tokio::test]
async fn a_sub_identity_reads_apart_from_its_principal() {
    let (dir, broker) = temp_broker();
    let mut alice = hello(&broker, "alice", dir.path()).await;
    broker
        .handle(
            CommsRequest::AgentPost {
                sub: None,
                body: new_thread(&[]),
            },
            &mut alice,
            &CancellationToken::new(),
        )
        .await;
    let read = broker
        .handle(
            CommsRequest::AgentInbox {
                sub: Some(SubName::parse("worker").expect("sub")),
                body: InboxBody::default(),
                wait_secs: 0,
            },
            &mut alice,
            &CancellationToken::new(),
        )
        .await;
    let CommsResponse::AgentRead(inbox) = read else {
        panic!("not read: {read:?}");
    };
    assert_eq!(
        inbox.messages.len(),
        1,
        "the parent's post is unread for its sub-identity"
    );
}

#[tokio::test]
async fn a_wait_beyond_the_limit_is_refused_before_it_waits() {
    let (dir, broker) = temp_broker();
    let mut alice = hello(&broker, "alice", dir.path()).await;
    let read = broker
        .handle(
            CommsRequest::AgentInbox {
                sub: None,
                body: InboxBody::default(),
                wait_secs: 41,
            },
            &mut alice,
            &CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(read, CommsResponse::AgentRefused(CommsError::WaitOutOfRange { .. })),
        "{read:?}"
    );
}

#[tokio::test]
async fn status_names_the_store_and_the_links_repository() {
    let (dir, broker) = temp_broker();
    let mut alice = hello(&broker, "alice", dir.path()).await;
    broker
        .handle(
            CommsRequest::AgentPost {
                sub: None,
                body: new_thread(&[]),
            },
            &mut alice,
            &CancellationToken::new(),
        )
        .await;
    let status = broker
        .handle(CommsRequest::AgentStatus, &mut alice, &CancellationToken::new())
        .await;
    let CommsResponse::AgentStatus(status) = status else {
        panic!("no status: {status:?}");
    };
    assert_eq!(status.state, "open");
    assert!(status.repo.is_some());
    let listed = broker
        .handle(
            CommsRequest::AgentThreads {
                sub: None,
                query: ThreadQuery::default(),
            },
            &mut alice,
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(listed, CommsResponse::AgentThreads(_)), "{listed:?}");
}

#[tokio::test]
async fn a_second_hello_on_a_link_is_refused_and_keeps_the_first_identity() {
    let (dir, broker) = temp_broker();
    let mut session = hello(&broker, "alice", dir.path()).await;
    let other = tempfile::tempdir().expect("tempdir");
    let again = broker
        .handle(
            CommsRequest::Hello {
                proto_ver: PROTO_VER,
                principal: Some(Principal::parse("mallory").expect("principal")),
                root: Some(other.path().to_path_buf()),
                host: None,
            },
            &mut session,
            &CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(again, CommsResponse::Error { ref code, .. } if code == "hello_repeated"),
        "{again:?}"
    );
    assert_eq!(session.principal.as_ref().map(Principal::as_str), Some("alice"));
    assert_eq!(session.root.as_deref(), Some(dir.path()));
}
