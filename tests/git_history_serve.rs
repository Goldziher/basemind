//! A `serve` session's git-history tools must be INDEX-backed, in every deployment.
//!
//! This file is NOT feature-gated on purpose — it runs against whichever `basemind serve` the build
//! produces, and the contract must hold for both:
//!
//! * **default build** (no daemon exists): serve is the sole writer, opens `git-history.fjall/`
//!   itself and builds it in-process. Unchanged, and the least-surprising standalone behavior.
//! * **`comms` build**: serve is read-only and forwards writes to the daemon, so the DAEMON builds
//!   the index and answers serve's history queries from it. Serve must never do that build itself —
//!   on a deep monorepo it is a multi-GB, minutes-long walk, and it would land in the process an
//!   agent is actively querying, once per session.
//!
//! Either way the observable is the same, which is the point: the index gets built, and history
//! tools use it.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;

/// A commit-message body token that exists ONLY in a commit body — never in a summary, an author
/// name, or a file. The live-walk fallback in `search_git_history` searches summaries + authors of a
/// bounded window and never loads bodies, so a hit on this token can ONLY come from the git-history
/// index's full-text posting lists. It is the observable that separates "index-backed" from
/// "degraded to the live walk".
const BODY_ONLY_TOKEN: &str = "zzqqxindexonlytoken";

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// A three-commit repo whose middle commit carries [`BODY_ONLY_TOKEN`] in its message BODY.
fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);

    std::fs::write(root.join("b.rs"), b"pub fn beta() {}\n").expect("write b.rs");
    git(root, &["add", "."]);
    git(
        root,
        &[
            "commit",
            "-qm",
            "add beta",
            "-m",
            &format!("body line {BODY_ONLY_TOKEN} here"),
        ],
    );

    std::fs::write(root.join("a.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("rewrite a.rs");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "widen alpha"]);
    dir
}

fn decode_text(result: &CallToolResult) -> Value {
    use rmcp::model::ContentBlock;
    let raw = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

/// Spawn a real `basemind serve` and complete the rmcp handshake. On a `comms` build this session is
/// `read_only` + `daemon_writer`; on a default build it is the local writer.
async fn spawn_server(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// A serve session's history tools answer from the git-history index — never from a permanent
/// live-walk fallback. Whoever legitimately owns the index in this build (serve itself standalone,
/// the daemon on a `comms` build) must build it, and serve must then use it.
///
/// Observable: `search_git_history` sets `partial: true` and searches only summaries/authors over a
/// bounded window when it falls back to the live walk. A hit on [`BODY_ONLY_TOKEN`] — which exists
/// only in a commit BODY — with `partial` unset therefore proves the answer came from the index.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_answers_git_history_from_the_index_not_the_live_walk() {
    let dir = build_repo();
    let root = dir.path();

    let service = spawn_server(root).await;
    let peer = service.peer().clone();

    // The daemon builds the index in the background; poll until it is fresh (bounded).
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut last;
    loop {
        let result = peer
            .call_tool(call_params(
                "search_git_history",
                json!({ "pattern": BODY_ONLY_TOKEN, "limit": 5 }),
            ))
            .await
            .expect("search_git_history call");
        last = decode_text(&result);
        let indexed = !last.get("partial").and_then(Value::as_bool).unwrap_or(false);
        let hits = last.get("commits").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        if indexed && hits > 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "serve never answered search_git_history from the git-history index \
                 (expected a body-only hit with partial=false); last response: {last}"
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let commits = last.get("commits").and_then(Value::as_array).expect("commits array");
    assert_eq!(commits.len(), 1, "exactly the one body-carrying commit matches: {last}");
    assert_eq!(
        commits[0].get("summary").and_then(Value::as_str),
        Some("add beta"),
        "the indexed hit is the commit whose BODY carries the token: {last}"
    );

    let _ = service.cancel().await;
}
