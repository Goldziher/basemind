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
use serde_json::{Value, json};
use tempfile::TempDir;
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
    basemind::store::init_isolated_cache();
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

/// The hits of a `code_refs` response as rows, each with its `path`: the tool lists them under their
/// file (`{"<path>": [hit, ...]}`).
#[cfg(all(
    feature = "comms",
    any(feature = "code-intel-js", feature = "code-intel-stack"),
    any(unix, windows)
))]
fn hit_rows(body: &Value) -> Vec<Value> {
    body.get("hits")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("hits are grouped by file: {body}"))
        .iter()
        .flat_map(|(path, rows)| {
            rows.as_array().expect("a file's hits").iter().map(move |row| {
                let mut row = row.clone();
                row["path"] = json!(path);
                row
            })
        })
        .collect()
}

fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

/// Panic when a verb tool reported a failure: those arrive as `isError` results, not protocol errors.
#[cfg(feature = "memory")]
fn expect_success(result: CallToolResult, what: &str) -> CallToolResult {
    assert_ne!(result.is_error, Some(true), "{what} failed: {result:?}");
    result
}

/// Spawn a basemind serve process (plain, ALWAYS-WRITABLE local topology) and complete the rmcp
/// handshake.
///
/// Returns the `RunningService`. Callers that need concurrent access should
/// clone the inner `Peer` via `service.peer().clone()` — `Peer` is `Clone`
/// and `call_tool` takes `&self`, so many tasks can share a single `Peer`.
/// To tear down, call `service.cancel().await` (takes ownership).
///
/// This is the local-writer / read-only-fallback shape (`serve_in_memory`), NOT a daemon-hosted
/// session — tests that assert daemon semantics must use [`spawn_hosted_server`] instead.
async fn spawn_server(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// Eight concurrent `git_log {path, symbol}` calls for the same symbol hit the shared
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
    for _ in 0_u8..8 {
        let p = Arc::clone(&peer);
        set.spawn(async move {
            p.call_tool(call_params(
                "git_log",
                json!({ "path": "a.rs", "symbol": "alpha", "limit": 10 }),
            ))
            .await
            .expect("git_log symbol history call")
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

/// A reader loop (10x `code_symbols`) racing an `index_rescan` write exercises the
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
                .call_tool(call_params("code_symbols", json!({ "name": "alp", "limit": 50 })))
                .await
                .unwrap_or_else(|error| panic!("code_symbols iteration {iteration} failed: {error}"));
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
            .call_tool(call_params("index_rescan", json!({})))
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

/// Four concurrent `memory_save` writers and four concurrent `memory_search`
/// readers exercise the `LanceStore` / `OnceCell` init path under contention.
///
/// Gated on `--features memory` because `memory_save` is a feature-missing error without it
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
            let result = p
                .call_tool(call_params(
                    "memory_save",
                    json!({
                        "key": format!("concurrency_test_k_{n}"),
                        "value": format!("concurrency_test_v_{n}"),
                    }),
                ))
                .await
                .unwrap_or_else(|e| panic!("memory_save {n} failed: {e}"));
            expect_success(result, &format!("memory_save {n}"));
        });
    }
    for n in 0_u32..4 {
        let p = Arc::clone(&peer);
        set.spawn(async move {
            let result = p
                .call_tool(call_params(
                    "memory_search",
                    json!({ "query": "concurrency_test", "limit": 10 }),
                ))
                .await
                .unwrap_or_else(|e| panic!("memory_search {n} failed: {e}"));
            expect_success(result, &format!("memory_search {n}"));
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
        .call_tool(call_params("memory_search", json!({ "prefix": "concurrency_test_k_" })))
        .await
        .expect("memory_search listing");
    let body = decode_text(&list_result);
    let entries = body
        .get("entries")
        .and_then(Value::as_array)
        .expect("memory_search listing missing 'entries' field");
    assert_eq!(
        entries.len(),
        4,
        "expected 4 memory entries with prefix 'concurrency_test_k_', got: {body}"
    );

    let _ = service.cancel().await;
}

/// Four concurrent `git_blame` calls for the same file exercise the shared
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
            p.call_tool(call_params("git_blame", json!({ "path": "a.rs" })))
                .await
                .unwrap_or_else(|error| panic!("git_blame task {i} failed: {error}"))
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
                "task {completed}: git_blame should return at least one hunk for a.rs"
            );
            completed += 1;
        }
    })
    .await
    .expect("parallel git_blame timed out after 60 s");

    assert_eq!(completed, 4, "all 4 concurrent git_blame calls should have completed");

    let _ = service.cancel().await;
}

/// Concurrent `memory_save` calls for the SAME key must serialize so the
/// read-modify-write of `created_at` is atomic. Without the per-key lock, two
/// puts both observe "no existing record" and stamp different `created_at`
/// values; whichever Fjall write lands last wins, so the surviving record's
/// `created_at` is non-deterministic.
///
/// Here we seed the key once (establishing `created_at = C0`), then fire 8
/// concurrent puts of new values. Every put must preserve `C0`, and a final
/// `memory_search {key}` must report `created_at == C0` with `updated_at >= C0`.
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
        .call_tool(call_params("memory_save", json!({ "key": KEY, "value": "seed" })))
        .await
        .expect("seed memory_save");
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
                    "memory_save",
                    json!({ "key": KEY, "value": format!("v{n}") }),
                ))
                .await
                .unwrap_or_else(|e| panic!("memory_save {n} failed: {e}"));
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
        .call_tool(call_params("memory_search", json!({ "key": KEY })))
        .await
        .expect("memory_search by key");
    let body = decode_text(&get_result);
    let created_final = body
        .get("created_at")
        .and_then(Value::as_i64)
        .expect("memory_search by key missing created_at");
    let updated_final = body
        .get("updated_at")
        .and_then(Value::as_i64)
        .expect("memory_search by key missing updated_at");
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

/// The refusal a `code_refs` scan gives in a session that holds no index: an error result naming
/// the fix, never an answer from anything but the index.
fn assert_refused_for_lack_of_an_index(result: &CallToolResult, what: &str) {
    use rmcp::model::ContentBlock;
    assert_eq!(result.is_error, Some(true), "{what} must be refused: {result:?}");
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    assert!(text.contains("holds this workspace's index"), "{what}: {text}");
    #[cfg(all(feature = "comms", any(unix, windows)))]
    assert!(text.contains("`basemind comms start`"), "{what}: {text}");
}

/// Multi-SESSION: two `serve` processes against the SAME repo. fjall is single-holder, so the
/// second process can't open the index and serves read-only (`index_db = None`). Its `code_refs`
/// call sites are refused naming the fix (ADR-0041) rather than answered from a partial in-RAM
/// projection of the blobs; it still answers, and does not block, on everything else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_session_refuses_references_without_an_index() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let serve1 = spawn_server(root).await;
    let serve2 = spawn_server(root).await;
    let peer2 = serve2.peer().clone();

    let result = peer2
        .call_tool(call_params("code_refs", json!({ "name": "alpha" })))
        .await
        .expect("code_refs answers on the read-only 2nd session");
    assert_refused_for_lack_of_an_index(&result, "code_refs on the 2nd session");
    let symbols = peer2
        .call_tool(call_params("code_symbols", json!({ "name": "alpha" })))
        .await
        .expect("code_symbols on the 2nd session");
    assert_ne!(symbols.is_error, Some(true), "{symbols:?}");

    let _ = serve1.cancel().await;
    let _ = serve2.cancel().await;
}

/// `code_refs` relation `implementations` scans the index too, and is refused the same way on the
/// read-only 2nd session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_session_refuses_implementations_without_an_index() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    let serve1 = spawn_server(root).await;
    let serve2 = spawn_server(root).await;
    let peer2 = serve2.peer().clone();

    let fi = peer2
        .call_tool(call_params(
            "code_refs",
            json!({ "name": "Drawable", "relation": "implementations" }),
        ))
        .await
        .expect("code_refs implementations on 2nd session");
    assert_refused_for_lack_of_an_index(&fi, "code_refs implementations on the 2nd session");

    let _ = serve1.cancel().await;
    let _ = serve2.cancel().await;
}

/// Extract symbol names from a `code_symbols` response body.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn symbol_names(body: &Value) -> Vec<String> {
    body.get("results")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The isolated comms dir `init_isolated_cache` pointed this process — and every child it spawns —
/// at, so the daemon, its socket, and every index stay in a per-process tempdir. Mirrors
/// `tests/git_history_daemon.rs`.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn comms_paths() -> basemind::comms::singleton::CommsPaths {
    basemind::comms::singleton::resolve_paths().expect("resolve the isolated comms paths")
}

/// Bring up a REAL `basemind comms daemon` and wait for it to answer.
///
/// Never call `CommsClient::ensure_and_connect` from a TEST: its spawn strategy execs
/// `current_exe()`, which here is the test harness binary — libtest reads `comms daemon` as a filter
/// argument and re-runs the whole suite, which spawns another "daemon", and so on. Spawn the real
/// binary explicitly instead. Idempotent: a second daemon on the same socket loses the bind race and
/// exits, so racing tests converge on one. Mirrors `tests/git_history_daemon.rs`.
#[cfg(all(feature = "comms", any(unix, windows)))]
#[allow(clippy::zombie_processes)]
fn ensure_real_daemon() {
    let paths = comms_paths();
    if basemind::comms::singleton::probe_alive(&paths.socket_path) {
        return;
    }
    let _child = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["comms", "daemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn comms daemon");
    // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    while std::time::Instant::now() < deadline {
        if basemind::comms::singleton::probe_alive(&paths.socket_path) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("comms daemon did not become ready");
}

/// Spawn a real `basemind serve`: a stdio relay to the daemon, which hosts the session over the
/// workspace it holds (the sole fjall writer). Unlike [`spawn_server`] (always writable, local), its
/// writes and resolved reads land in the daemon brought up by [`ensure_real_daemon`].
#[cfg(all(feature = "comms", any(unix, windows)))]
async fn spawn_hosted_server(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};

    ensure_real_daemon();
    let command = tokio::process::Command::new(env!("CARGO_BIN_EXE_basemind")).configure(|command| {
        command
            .arg("--root")
            .arg(root)
            .arg("serve")
            .arg("--view")
            .arg("working");
    });
    let transport = TokioChildProcess::new(command).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake over the relay")
}

/// A comms-build `serve` is hosted by the machine daemon (the sole fjall writer). From an EMPTY
/// index, `index_rescan` makes the daemon scan and the session's map is rebuilt from what it wrote,
/// so `code_symbols` returns fresh symbols. A second serve on the same repo answers queries AND rescans too — N
/// sessions read + write with no single-holder downgrade (the whole point of the daemon model).
#[cfg(all(feature = "comms", any(unix, windows)))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_serve_rescans_through_the_daemon_and_sees_fresh_symbols() {
    let dir = build_repo();
    let root = dir.path();

    let serve_a = spawn_hosted_server(root).await;
    let peer_a = serve_a.peer().clone();

    let rescan = peer_a
        .call_tool(call_params("index_rescan", json!({})))
        .await
        .expect("rescan A forwarded to daemon");
    let scanned = decode_text(&rescan)
        .get("scanned")
        .and_then(Value::as_u64)
        .expect("rescan response missing 'scanned'");
    assert!(
        scanned >= 4,
        "daemon scanned the fixture's 4 source files, got {scanned}"
    );

    let search_a = peer_a
        .call_tool(call_params("code_symbols", json!({ "name": "alpha", "limit": 10 })))
        .await
        .expect("search A");
    let names_a = symbol_names(&decode_text(&search_a));
    assert!(
        names_a.iter().any(|n| n == "alpha"),
        "serve A sees 'alpha' after the daemon-forwarded scan: {names_a:?}"
    );

    let serve_b = spawn_hosted_server(root).await;
    let peer_b = serve_b.peer().clone();
    let search_b = peer_b
        .call_tool(call_params("code_symbols", json!({ "name": "Beta", "limit": 10 })))
        .await
        .expect("search B");
    let names_b = symbol_names(&decode_text(&search_b));
    assert!(
        names_b.iter().any(|n| n == "Beta"),
        "serve B reads 'Beta' from the shared index: {names_b:?}"
    );
    let rescan_b = peer_b
        .call_tool(call_params("index_rescan", json!({})))
        .await
        .expect("rescan B also forwards + succeeds");
    let scanned_b = decode_text(&rescan_b)
        .get("scanned")
        .and_then(Value::as_u64)
        .expect("rescan B response missing 'scanned'");
    assert!(scanned_b >= 4, "serve B rescan re-scans the fixture, got {scanned_b}");

    let _ = serve_a.cancel().await;
    let _ = serve_b.cancel().await;
}

/// A 2-file TypeScript repo with a genuine CROSS-FILE resolved reference: `lib.ts` exports
/// `target`, `main.ts` imports it and calls it twice. oxc (code-intel-js) records those calls in the
/// cross-file `refs_by_def` reverse index at scan time.
#[cfg(all(feature = "comms", feature = "code-intel-js", any(unix, windows)))]
fn build_ts_crossfile_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("lib.ts"), b"export function target() { return 1; }\n").unwrap();
    std::fs::write(
        root.join("main.ts"),
        b"import { target } from \"./lib\";\ntarget();\ntarget();\n",
    )
    .unwrap();
    git(root, &["add", "lib.ts", "main.ts"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

/// Regression for the daemon-era tsg/code-intel loss: a session that holds no fjall index cannot see
/// the cross-file `refs_by_def` reverse index (written by the scanner's resolve pass, held only in
/// the daemon's index), so precise `find_callers` degraded to `resolved: false` with only the
/// name-based scan. A daemon-hosted session resolves through the daemon's pool and reports the
/// cross-file callers again. Uses TypeScript (oxc)
/// so it runs on any `code-intel-js` build without the heavier Python/Java stack-graph engine.
#[cfg(all(feature = "comms", feature = "code-intel-js", any(unix, windows)))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_serve_resolves_cross_file_callers_through_the_daemon() {
    let dir = build_ts_crossfile_repo();
    let root = dir.path();

    let serve = spawn_hosted_server(root).await;
    let peer = serve.peer().clone();

    let rescan = peer
        .call_tool(call_params("index_rescan", json!({})))
        .await
        .expect("rescan forwarded to daemon");
    let scanned = decode_text(&rescan).get("scanned").and_then(Value::as_u64).unwrap_or(0);
    assert!(scanned >= 2, "daemon scanned both TS files, got {scanned}");

    let body = decode_text(
        &peer
            .call_tool(call_params(
                "code_refs",
                json!({ "name": "target", "path": "lib.ts", "limit": 50 }),
            ))
            .await
            .expect("find_callers on the cross-file definition"),
    );

    let resolved_total = body.get("resolved_total").and_then(Value::as_u64).unwrap_or(0);
    assert!(
        resolved_total >= 1,
        "find_callers must resolve cross-file through the daemon (not degrade to the name scan): {body}"
    );
    let hit_paths: Vec<String> = hit_rows(&body)
        .iter()
        .filter_map(|h| h.get("path").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert!(
        hit_paths.iter().any(|p| p == "main.ts"),
        "the resolved caller in main.ts must be reported, got hit paths: {hit_paths:?}"
    );

    let _ = serve.cancel().await;
}

/// The same cross-file `find_callers` resolution, but for PYTHON (the tsg stack-graph engine) —
/// `lib.py` defines `target`, `main.py` imports and calls it. Proves the daemon-hosted path
/// restores precise cross-file resolution for the stack-graph languages too (the grantflow target),
/// not just oxc JS/TS. Gated on `code-intel-stack`, so it only runs where the Python engine is in.
#[cfg(all(feature = "comms", feature = "code-intel-stack", any(unix, windows)))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_serve_resolves_cross_file_python_callers_through_the_daemon() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("lib.py"), b"def target():\n    return 1\n").unwrap();
    std::fs::write(root.join("main.py"), b"from lib import target\ntarget()\ntarget()\n").unwrap();
    git(root, &["add", "lib.py", "main.py"]);
    git(root, &["commit", "-qm", "init"]);

    let serve = spawn_hosted_server(root).await;
    let peer = serve.peer().clone();

    let rescan = peer
        .call_tool(call_params("index_rescan", json!({})))
        .await
        .expect("rescan forwarded to daemon");
    let scanned = decode_text(&rescan).get("scanned").and_then(Value::as_u64).unwrap_or(0);
    assert!(scanned >= 2, "daemon scanned both Python files, got {scanned}");

    let body = decode_text(
        &peer
            .call_tool(call_params(
                "code_refs",
                json!({ "name": "target", "path": "lib.py", "limit": 50 }),
            ))
            .await
            .expect("find_callers on the cross-file Python definition"),
    );
    let resolved_total = body.get("resolved_total").and_then(Value::as_u64).unwrap_or(0);
    assert!(
        resolved_total >= 1,
        "Python find_callers must resolve cross-file through the daemon: {body}"
    );
    let hit_paths: Vec<String> = hit_rows(&body)
        .iter()
        .filter_map(|h| h.get("path").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert!(
        hit_paths.iter().any(|p| p == "main.py"),
        "the resolved caller in main.py must be reported, got hit paths: {hit_paths:?}"
    );

    let _ = serve.cancel().await;
}
