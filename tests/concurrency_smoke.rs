//! Concurrency smoke tests for the MCP server.
//!
//! Exercises concurrent tool calls against a single running `basemind serve`
//! subprocess to catch deadlocks, torn reads, and thread-safety issues that the
//! sequential `tests/mcp_smoke.rs` cannot reach. Each test spins up its own
//! throwaway git repo + scan + server so they are fully independent.
//!
//! All assertions are deterministic — no timing-sensitive ordering checks.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;
use tokio::task::JoinSet;
use tokio::time::timeout;

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

/// Build a tiny two-commit git repo with a Rust + TypeScript file.
///
/// The second commit modifies `alpha()` so that `symbol_history` has at least
/// one "modified" entry to return.
fn build_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\npub struct Beta { x: i32 }\n").unwrap();
    std::fs::write(root.join("b.ts"), b"export function plain() { return 1; }\n").unwrap();
    std::fs::write(
        root.join("c.rs"),
        b"pub fn caller() { alpha(); alpha(); other(); alpha(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("d.rs"),
        b"trait Drawable { fn draw(&self); }\npub struct Circle;\nimpl Drawable for Circle { fn draw(&self) { alpha(); } }\n",
    )
    .unwrap();
    git(root, &["add", "a.rs", "b.ts", "c.rs", "d.rs"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() { let _ = 1; }\npub struct Beta { x: i32 }\n",
    )
    .unwrap();
    git(root, &["commit", "-aqm", "tweak alpha"]);
    dir
}

fn run_scan(root: &Path) {
    // These tests call `scan` synchronously from inside a `#[tokio::test]` multi-thread runtime.
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    basemind::scanner::scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .expect("scan");
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

/// Spawn a basemind serve process and complete the rmcp handshake.
///
/// Returns the `RunningService`. Callers that need concurrent access should
/// clone the inner `Peer` via `service.peer().clone()` — `Peer` is `Clone`
/// and `call_tool` takes `&self`, so many tasks can share a single `Peer`.
/// To tear down, call `service.cancel().await` (takes ownership).
async fn spawn_server(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// Eight concurrent `symbol_history` calls for the same symbol hit the shared
/// `Mutex<LruCache>` in `ServerState::outline_cache` at the same time.
///
/// Asserts: all 8 calls complete within 60 s, each returns a `history` array,
/// no panics / deadlocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_symbol_history_same_symbol() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let service = spawn_server(root).await;
    let peer = Arc::new(service.peer().clone());

    let mut set: JoinSet<CallToolResult> = JoinSet::new();
    for i in 0_u8..8 {
        let p = Arc::clone(&peer);
        let hash_mode = if i < 4 { "normalized" } else { "structural" };
        set.spawn(async move {
            p.call_tool(call_params(
                "symbol_history",
                json!({
                    "path": "a.rs",
                    "name": "alpha",
                    "limit": 10,
                    "hash_mode": hash_mode
                }),
            ))
            .await
            .expect("symbol_history call")
        });
    }

    let mut completed = 0_usize;
    timeout(Duration::from_secs(60), async {
        while let Some(result) = set.join_next().await {
            let call_result = result.expect("task panicked");
            let body = decode_text(&call_result);
            assert!(
                body.get("history").and_then(Value::as_array).is_some(),
                "call {completed}: expected history array, got: {body}"
            );
            completed += 1;
        }
    })
    .await
    .expect("parallel symbol_history timed out after 60 s");

    assert_eq!(completed, 8, "all 8 concurrent calls should have completed");

    let _ = service.cancel().await;
}

/// A reader loop (10x `search_symbols`) racing a `rescan` write exercises the
/// `RwLock<Store>` write-fairness path.
///
/// Asserts: all 10 reader calls complete without error; the single rescan also
/// completes; no deadlock within 60 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_search_and_rescan() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let service = spawn_server(root).await;
    let peer_a = service.peer().clone();
    let peer_b = service.peer().clone();

    let task_a = tokio::spawn(async move {
        for iteration in 0_u32..10 {
            let result = peer_a
                .call_tool(call_params("search_symbols", json!({ "needle": "alp", "limit": 50 })))
                .await
                .unwrap_or_else(|error| panic!("search_symbols iteration {iteration} failed: {error}"));
            let body = decode_text(&result);
            assert!(
                body.get("results").and_then(Value::as_array).is_some(),
                "iteration {iteration}: expected results array, got: {body}"
            );
        }
    });

    let task_b = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let result = peer_b
            .call_tool(call_params("rescan", json!({})))
            .await
            .expect("rescan call");
        let body = decode_text(&result);
        let scanned = body
            .get("scanned")
            .and_then(Value::as_u64)
            .expect("rescan response missing 'scanned' field");
        assert!(scanned > 0, "rescan should walk at least the fixture files");
    });

    timeout(Duration::from_secs(60), async {
        let (a, b) = tokio::join!(task_a, task_b);
        a.expect("task A panicked");
        b.expect("task B panicked");
    })
    .await
    .expect("concurrent_search_and_rescan timed out after 60 s");

    let _ = service.cancel().await;
}

/// Four concurrent `memory_put` writers and four concurrent `memory_search`
/// readers exercise the `LanceStore` / `OnceCell` init path under contention.
///
/// Gated on `--features memory` because `memory_put` is a no-op error without it
/// and the `LanceStore` won't be initialised.
#[cfg(feature = "memory")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_memory_put_and_search() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let service = spawn_server(root).await;
    let peer = Arc::new(service.peer().clone());

    let mut set: JoinSet<()> = JoinSet::new();
    for n in 0_u32..4 {
        let p = Arc::clone(&peer);
        set.spawn(async move {
            p.call_tool(call_params(
                "memory_put",
                json!({
                    "key": format!("concurrency_test_k_{n}"),
                    "value": format!("concurrency_test_v_{n}"),
                    "embed": false
                }),
            ))
            .await
            .unwrap_or_else(|e| panic!("memory_put {n} failed: {e}"));
        });
    }
    for n in 0_u32..4 {
        let p = Arc::clone(&peer);
        set.spawn(async move {
            p.call_tool(call_params(
                "memory_search",
                json!({ "query": "concurrency_test", "limit": 10 }),
            ))
            .await
            .unwrap_or_else(|e| panic!("memory_search {n} failed: {e}"));
        });
    }

    timeout(Duration::from_secs(120), async {
        while let Some(result) = set.join_next().await {
            result.expect("memory task panicked");
        }
    })
    .await
    .expect("parallel_memory_put_and_search timed out");

    let list_result = peer
        .call_tool(call_params("memory_list", json!({ "prefix": "concurrency_test_k_" })))
        .await
        .expect("memory_list");
    let body = decode_text(&list_result);
    let entries = body
        .get("entries")
        .and_then(Value::as_array)
        .expect("memory_list response missing 'entries' field");
    assert_eq!(
        entries.len(),
        4,
        "expected 4 memory entries with prefix 'concurrency_test_k_', got: {body}"
    );

    let _ = service.cancel().await;
}

/// Four concurrent `blame_file` calls for the same file exercise the shared
/// `GitCache` under parallel load.
///
/// Asserts: all 4 calls return a non-empty `hunks` array; no panics or deadlocks
/// within 60 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_blame_same_file() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let service = spawn_server(root).await;
    let peer = Arc::new(service.peer().clone());

    let mut set: JoinSet<CallToolResult> = JoinSet::new();
    for i in 0_u8..4 {
        let p = Arc::clone(&peer);
        set.spawn(async move {
            p.call_tool(call_params("blame_file", json!({ "path": "a.rs" })))
                .await
                .unwrap_or_else(|error| panic!("blame_file task {i} failed: {error}"))
        });
    }

    let mut completed = 0_usize;
    timeout(Duration::from_secs(60), async {
        while let Some(result) = set.join_next().await {
            let call_result = result.expect("task panicked");
            let body = decode_text(&call_result);
            let hunks = body
                .get("hunks")
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("task {completed}: expected hunks array, got: {body}"));
            assert!(
                !hunks.is_empty(),
                "task {completed}: blame_file should return at least one hunk for a.rs"
            );
            completed += 1;
        }
    })
    .await
    .expect("parallel blame_file timed out after 60 s");

    assert_eq!(completed, 4, "all 4 concurrent blame_file calls should have completed");

    let _ = service.cancel().await;
}

/// Concurrent `memory_put` calls for the SAME key must serialize so the
/// read-modify-write of `created_at` is atomic. Without the per-key lock, two
/// puts both observe "no existing record" and stamp different `created_at`
/// values; whichever Fjall write lands last wins, so the surviving record's
/// `created_at` is non-deterministic.
///
/// Here we seed the key once (establishing `created_at = C0`), then fire 8
/// concurrent puts of new values. Every put must preserve `C0`, and a final
/// `memory_get` must report `created_at == C0` with `updated_at >= C0`.
#[cfg(feature = "memory")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_key_put_preserves_created_at() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let service = spawn_server(root).await;
    let peer = Arc::new(service.peer().clone());

    const KEY: &str = "same_key_race";

    let seed = peer
        .call_tool(call_params(
            "memory_put",
            json!({ "key": KEY, "value": "seed", "embed": false }),
        ))
        .await
        .expect("seed memory_put");
    let seed_body = decode_text(&seed);
    let created0 = seed_body
        .get("created_at")
        .and_then(Value::as_i64)
        .expect("seed put missing created_at");

    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut set: JoinSet<i64> = JoinSet::new();
    for n in 0_u32..8 {
        let p = Arc::clone(&peer);
        set.spawn(async move {
            let result = p
                .call_tool(call_params(
                    "memory_put",
                    json!({ "key": KEY, "value": format!("v{n}"), "embed": false }),
                ))
                .await
                .unwrap_or_else(|e| panic!("memory_put {n} failed: {e}"));
            decode_text(&result)
                .get("created_at")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| panic!("put {n} missing created_at"))
        });
    }

    timeout(Duration::from_secs(60), async {
        while let Some(result) = set.join_next().await {
            let created = result.expect("put task panicked");
            assert_eq!(
                created, created0,
                "concurrent same-key put must preserve the original created_at"
            );
        }
    })
    .await
    .expect("concurrent_same_key_put timed out after 60 s");

    let get_result = peer
        .call_tool(call_params("memory_get", json!({ "key": KEY })))
        .await
        .expect("memory_get");
    let body = decode_text(&get_result);
    let created_final = body
        .get("created_at")
        .and_then(Value::as_i64)
        .expect("memory_get missing created_at");
    let updated_final = body
        .get("updated_at")
        .and_then(Value::as_i64)
        .expect("memory_get missing updated_at");
    assert_eq!(
        created_final, created0,
        "final record must keep the original created_at after the put storm"
    );
    assert!(
        updated_final >= created0,
        "updated_at ({updated_final}) must be >= created_at ({created0})"
    );

    let _ = service.cancel().await;
}

/// Multi-SESSION repro: two `serve` processes against the SAME repo. fjall is
/// single-holder, so the second process can't open the index and falls back to
/// read-only (`index_db = None`). It must STILL answer `find_references` — from
/// the in-RAM call index built off the shared, concurrently-readable blobs.
///
/// Before the fix this returned a `read_only_index_unavailable` error (the
/// reported "blocked / not responsive"); now it resolves the references.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_session_resolves_find_references_from_blobs() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let serve1 = spawn_server(root).await;
    let serve2 = spawn_server(root).await;
    let peer2 = serve2.peer().clone();

    let result = peer2
        .call_tool(call_params(
            "find_references",
            json!({ "name": "alpha", "format": "json" }),
        ))
        .await
        .expect("find_references must succeed on the read-only 2nd session");
    let body = decode_text(&result);
    let total = body
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("find_references response missing 'total': {body}"));
    assert!(
        total >= 3,
        "2nd (read-only) session must resolve alpha()'s 3 call sites from blobs, got: {body}"
    );

    let _ = serve1.cancel().await;
    let _ = serve2.cancel().await;
}

/// The other two Fjall-backed tools — `call_graph` and `find_implementations` —
/// must also work on the read-only 2nd session, via the same in-RAM indexes built
/// from the shared blobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_session_resolves_call_graph_and_impls_from_blobs() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let serve1 = spawn_server(root).await;
    let serve2 = spawn_server(root).await;
    let peer2 = serve2.peer().clone();

    let cg = peer2
        .call_tool(call_params(
            "call_graph",
            json!({ "name": "alpha", "direction": "callers" }),
        ))
        .await
        .expect("call_graph on 2nd session");
    let cg_body = decode_text(&cg);
    let names: Vec<&str> = cg_body
        .get("nodes")
        .and_then(Value::as_array)
        .map(|ns| {
            ns.iter()
                .filter_map(|n| n.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        names.contains(&"caller"),
        "call_graph must find caller() among alpha()'s callers, got: {cg_body}"
    );

    let fi = peer2
        .call_tool(call_params("find_implementations", json!({ "trait_name": "Drawable" })))
        .await
        .expect("find_implementations on 2nd session");
    let fi_body = decode_text(&fi);
    let fi_total = fi_body
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("find_implementations missing total: {fi_body}"));
    assert!(
        fi_total >= 1,
        "2nd session must resolve the Drawable impl from blobs, got: {fi_body}"
    );

    let _ = serve1.cancel().await;
    let _ = serve2.cancel().await;
}
