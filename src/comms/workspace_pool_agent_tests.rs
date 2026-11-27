//! The agent tools of a daemon-hosted session, over a real rmcp session on a hosted read stack.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::json;

use super::{WorkspacePool, no_history, workspace_with_sources};
use crate::comms::agents_hub::AgentHub;
use crate::comms::v2::api::{Actor, Caller, PostBody, PostTarget, Principal, RepoKey};

/// Waiting reads must not hold the daemon's bounded blocking pool: with more waiters than blocking
/// threads, a post still lands and every waiter is answered at once. A wait that parked a blocking
/// thread for its whole length starved the post (and everything else) until the waits ran out.
#[test]
fn more_waiting_reads_than_blocking_threads_still_let_a_post_through() {
    const BLOCKING_THREADS: usize = 2;
    const WAITERS: usize = 4;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(BLOCKING_THREADS)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        crate::store::init_isolated_cache();
        let pool = Arc::new(WorkspacePool::new(super::DEFAULT_HOT_CAP));
        let workspace = workspace_with_sources();
        let shared = pool
            .get_or_build_serve_state(workspace.path(), no_history())
            .await
            .expect("build hosted read stack");
        let store_dir = tempfile::tempdir().expect("store dir");
        let hub = Arc::new(AgentHub::open(store_dir.path()));
        let repo = repo_key();
        let principal = Principal::parse("reader").expect("principal");
        let context = Arc::new(crate::mcp::AgentContext::new(
            Arc::clone(&hub),
            principal,
            repo.clone(),
            Vec::new(),
        ));
        let server = crate::mcp::BasemindServer::from_shared(
            shared,
            Arc::new(crate::mcp::admission::HeavyAdmission::default()),
            Some(context),
        );
        let (client_end, server_end) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            if let Ok(running) = server.serve(server_end).await {
                let _ = running.waiting().await;
            }
        });
        let client = Arc::new(().serve(client_end).await.expect("rmcp handshake"));

        let writer = Caller::new(Actor::parse("writer").expect("actor"), repo);
        let to_reader = PostBody {
            target: PostTarget::NewThread {
                subject: "status".to_string(),
                to: vec![Actor::parse("reader").expect("actor")],
                paths: Vec::new(),
            },
            body: "first".to_string(),
        };
        let thread = hub
            .post(writer.clone(), to_reader, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("post")
            .thread;
        // Read as the principal, so every sub-identity starts past the first post and waits.
        let reader = Caller::new(Actor::parse("reader").expect("actor"), repo_key());
        hub.read(
            reader,
            crate::comms::v2::api::InboxBody::default(),
            Duration::ZERO,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("read the first post");
        let mut waiters = tokio::task::JoinSet::new();
        for index in 0..WAITERS {
            let client = Arc::clone(&client);
            waiters.spawn(async move {
                let arguments = json!({ "as": format!("w{index}"), "wait_secs": 20, "peek": true });
                client
                    .call_tool(
                        CallToolRequestParams::new("agent_inbox")
                            .with_arguments(arguments.as_object().cloned().expect("object")),
                    )
                    .await
            });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        let post = PostBody {
            target: PostTarget::Thread(thread),
            body: "news".to_string(),
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            hub.post(writer, post, &tokio_util::sync::CancellationToken::new()),
        )
        .await
        .expect("a post is not starved by waiting reads")
        .expect("post");
        while let Some(answer) = tokio::time::timeout(Duration::from_secs(5), waiters.join_next())
            .await
            .expect("every waiter is answered once the post lands")
        {
            let result = answer.expect("waiter task").expect("tool call");
            assert_ne!(result.is_error, Some(true), "{result:?}");
        }
        assert!(started.elapsed() < Duration::from_secs(10));
    });
}

fn repo_key() -> RepoKey {
    RepoKey::parse("path:/hosted-test").expect("repo")
}
