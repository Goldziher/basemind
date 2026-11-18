//! Daemon lifecycle integration: state that must outlive a single daemon process.
//!
//! The scattered comms/concurrency smokes already pin the in-session guarantees — two `serve`
//! sessions on one repo both read AND write through the daemon (`concurrency_smoke::
//! daemon_writer_serve_forwards_rescan_and_sees_fresh_symbols`), the machine registry
//! auto-registers a Hello cwd (`comms_smoke::machine_registry_auto_registers_a_connecting_repo`),
//! and the blob GC reclaims only orphans (`schema_bump::schema_bump_refreshes_blobs_in_place_and_gc_
//! reclaims_only_orphans`). What none of them exercise is **durability across the daemon's own
//! lifecycle**: the registry is an atomic msgpack snapshot, so a repo registration must survive the
//! daemon exiting and a fresh daemon reloading the same `BASEMIND_DATA_HOME`. This test pins that
//! path end to end against a real detached daemon.

#![cfg(feature = "comms")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use basemind::comms::client::CommsClient;
use basemind::comms::http_frontend;
use basemind::comms::singleton::{CommsPaths, comms_socket_path, generation_dir, probe_alive};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// How long a just-started daemon may take to publish its HTTP address. It binds HTTP after it has
/// built its broker, which a contended disk delays well past the 10-15 s these tests used to allow.
const HTTP_READY: Duration = Duration::from_secs(90);

/// Owns a spawned daemon process so it is always reaped. Constructed twice per test to exercise a
/// restart on the same `comms_dir` / `BASEMIND_DATA_HOME`.
struct Daemon {
    child: Child,
    comms_dir: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    fn start(comms_dir: &Path) -> Self {
        let socket = comms_socket_path(&generation_dir(comms_dir));
        let child = Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", comms_dir)
            // Isolate the daemon's registry snapshot + index writes to the same tempdir so this ~keep
            // test never touches the real XDG cache, and a restart reloads the same state. ~keep
            .env("BASEMIND_DATA_HOME", comms_dir)
            // The HTTP front-end is opt-in (see `comms::http_auth`); these tests drive it, so the ~keep
            // spawned daemon gets the grant explicitly rather than inheriting an ambient one. ~keep
            .env(http_frontend::ALLOW_HTTP_ENV, "1")
            .env(http_frontend::HTTP_ADDR_ENV, "127.0.0.1:0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon");
        let daemon = Self {
            child,
            comms_dir: comms_dir.to_path_buf(),
            socket,
        };
        // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            if probe_alive(&daemon.socket) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("comms daemon did not become ready");
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stop the daemon and wait for the socket to go dead, so a restart on the same path binds
    /// cleanly instead of racing the outgoing process.
    fn stop(self) {
        let socket = self.socket.clone();
        drop(self);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !probe_alive(&socket) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("comms daemon did not release its socket after stop");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", &self.comms_dir)
            .output();
        if self.child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(200));
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

/// Wait until a scan of `repo` by the daemon whose data home is `data_home` is in flight (its
/// in-flight breadcrumb exists) or `done` says the request already finished. A fixed sleep raced the
/// daemon under load: the signal could land before the scan had begun, or after it had ended.
async fn scan_in_flight(data_home: &Path, repo: &Path, done: impl Fn() -> bool) {
    let workspace = data_home
        .join("cache")
        .join("workspaces")
        .join(basemind::store::workspace_key(repo));
    let breadcrumb = basemind::scan_evidence::inflight_path(&workspace);
    let deadline = Instant::now() + Duration::from_secs(120);
    while !breadcrumb.exists() && !done() {
        assert!(Instant::now() < deadline, "the rescan never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Connect a client whose Hello carries `root` as cwd, so the daemon auto-registers that workspace.
async fn connect(socket: &Path, agent: &str, root: &Path) -> CommsClient {
    let paths = CommsPaths {
        comms_dir: socket.parent().expect("socket parent").to_path_buf(),
        socket_path: socket.to_path_buf(),
    };
    let hello = basemind::comms::client::ClientHello::agent(
        basemind::comms::v2::Principal::parse(agent).expect("principal"),
        root.to_path_buf(),
        None,
    );
    CommsClient::connect(&paths, hello)
        .await
        .unwrap_or_else(|e| panic!("connect {agent}: {e}"))
}

/// Send one stateless MCP JSON-RPC call to the daemon's hosted HTTP frontend.
async fn hosted_tool_call(
    addr: &str,
    token: &str,
    root: &Path,
    agent: &str,
    id: usize,
    tool: &str,
    arguments: Value,
) -> Value {
    let call = tool.to_string();
    let target = format!("/mcp?root={}&agent={agent}", root.display());
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}
    })
    .to_string();
    let mut stream = TcpStream::connect(addr).await.expect("connect hosted MCP frontend");
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json, text/event-stream\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write hosted MCP request");
    stream.flush().await.expect("flush hosted MCP request");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read hosted MCP response");
    let response = String::from_utf8_lossy(&raw);
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("hosted MCP response has no status: {response}"));
    let response_body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
    assert_eq!(status, 200, "hosted {call} must return 200: {response_body}");
    assert!(
        !response_body.contains("SpawnTimeout")
            && !response_body.contains("spawned comms daemon")
            && !response_body.contains("contender"),
        "hosted {call} must not probe or spawn a daemon contender: {response_body}"
    );
    serde_json::from_str(response_body)
        .unwrap_or_else(|error| panic!("hosted {call} response is not JSON ({error}): {response_body}"))
}

/// Run a git command in `cwd`, asserting success.
fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A committed git repo on branch `main` with one source file, rooted at `main`.
fn init_git_repo(main: &Path) {
    std::fs::create_dir_all(main).expect("mkdir main");
    git(&["init", "-q", "-b", "main"], main);
    git(&["config", "user.email", "t@example.com"], main);
    git(&["config", "user.name", "Test"], main);
    std::fs::write(main.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    git(&["add", "."], main);
    git(&["commit", "-qm", "init"], main);
}

/// A committed git repo with `n_files` small Rust sources under `src/`, so a rescan does real
/// per-file extraction work — the substrate for the concurrency stress (vs the single-file
/// [`init_git_repo`]).
fn init_bulk_git_repo(main: &Path, n_files: usize) {
    std::fs::create_dir_all(main.join("src")).expect("mkdir src");
    git(&["init", "-q", "-b", "main"], main);
    git(&["config", "user.email", "t@example.com"], main);
    git(&["config", "user.name", "Test"], main);
    for i in 0..n_files {
        let body = format!(
            "pub fn f{i}() -> u32 {{ {i} }}\npub struct S{i};\nimpl S{i} {{ pub fn m{i}(&self) -> u32 {{ f{i}() }} }}\n"
        );
        std::fs::write(main.join("src").join(format!("m{i}.rs")), body).expect("write src file");
    }
    git(&["add", "."], main);
    git(&["commit", "-qm", "bulk"], main);
}

/// A committed git repo whose scan takes tens of seconds in a debug build: `n_files` sources of
/// `fns_per_file` functions each. The substrate for the SIGTERM-mid-scan regression, where the
/// in-flight scan must comfortably outlast the daemon's exit deadline — a small fixture would finish
/// before the drain grace and mask the pre-fix hang entirely. Feature-neutral (plain Rust sources).
#[cfg(unix)]
fn init_heavy_git_repo(main: &Path, n_files: usize, fns_per_file: usize) {
    std::fs::create_dir_all(main.join("src")).expect("mkdir src");
    git(&["init", "-q", "-b", "main"], main);
    git(&["config", "user.email", "t@example.com"], main);
    git(&["config", "user.name", "Test"], main);
    for i in 0..n_files {
        let mut body = String::with_capacity(fns_per_file * 80);
        for j in 0..fns_per_file {
            body.push_str(&format!(
                "pub fn f{i}_{j}(x: u32) -> u32 {{ let y = x + {j}; y.wrapping_mul({m}) }}\n",
                m = i % 97 + 1
            ));
        }
        body.push_str(&format!("pub struct S{i} {{ pub a: u32, pub b: String }}\n"));
        std::fs::write(main.join("src").join(format!("m{i}.rs")), body).expect("write src file");
    }
    git(&["add", "."], main);
    git(&["commit", "-qm", "heavy"], main);
}

/// Read a positive-integer stress knob from the environment, defaulting when unset or unparseable.
/// Lets the concurrency stress be cranked far harder on a big machine (`BASEMIND_STRESS_CLIENTS=64`).
fn stress_knob(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// Regression: stateless MCP requests hosted by the comms daemon must connect directly back to its
/// broker. They must not enter the singleton ensure/probe path from inside the daemon, which can
/// stall concurrent cold identities until `SpawnTimeout` while each request waits for a contender.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cold_hosted_calls_use_the_running_daemon() {
    const MIN_IDENTITIES_PER_TOOL: usize = 8;
    const MAX_IDENTITIES_PER_TOOL: usize = 32;
    const IDENTITIES_PER_WORKER: usize = 2;
    // ~keep The regression this guards against ends every stalled call with a `SpawnTimeout` error
    // ~keep after 30 s, which the per-response assertions below catch. The bound only stops a hang;
    // ~keep at 20 s it raced a cold daemon building the workspace for 16-32 calls under load.
    const COMPLETION_BOUND: Duration = Duration::from_secs(120);

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut observer = connect(&socket, "hosted-observer-before", &repo).await;
    let daemon_pid = observer.status().await.expect("status before hosted calls").pid;
    drop(observer);
    let addr = http_frontend::await_http_ready(&generation_dir(&comms_dir), HTTP_READY)
        .await
        .expect("daemon-hosted MCP transport ready");
    let token =
        http_frontend::published_token(&generation_dir(&comms_dir)).expect("the daemon publishes its bearer token");

    let worker_count = std::thread::available_parallelism().map_or(1, usize::from);
    let identities_per_tool = worker_count
        .saturating_mul(IDENTITIES_PER_WORKER)
        .clamp(MIN_IDENTITIES_PER_TOOL, MAX_IDENTITIES_PER_TOOL);
    let mut calls = tokio::task::JoinSet::new();
    for index in 0..identities_per_tool {
        for tool in ["index_status", "find_files"] {
            let addr = addr.clone();
            let token = token.clone();
            let repo = repo.clone();
            let agent = format!("hosted-{tool}-{index}");
            calls.spawn(async move {
                let response = hosted_tool_call(&addr, &token, &repo, &agent, index, tool, json!({})).await;
                (agent, tool, response)
            });
        }
    }

    let responses = tokio::time::timeout(COMPLETION_BOUND, async move {
        let mut responses = Vec::with_capacity(identities_per_tool * 2);
        while let Some(result) = calls.join_next().await {
            responses.push(result.expect("hosted tool task must not panic"));
        }
        responses
    })
    .await
    .expect("all concurrent cold hosted calls must complete within the bound");

    assert_eq!(responses.len(), identities_per_tool * 2);
    for (agent, tool, response) in responses {
        assert!(
            response.get("error").is_none() && response["result"].is_object(),
            "hosted {tool} for {agent} must succeed: {response}"
        );
        assert_ne!(
            response["result"]["isError"],
            Value::Bool(true),
            "hosted {tool} for {agent} returned a tool error: {response}"
        );
    }

    let mut observer = connect(&socket, "hosted-observer-after", &repo).await;
    let after_pid = observer.status().await.expect("status after hosted calls").pid;
    assert_eq!(after_pid, daemon_pid, "hosted calls must stay on the original daemon");
    drop(observer);
    daemon.stop();
}

/// A keyword search served inside the daemon must read the pool's live fjall index through the
/// in-process host seam. Forwarding over the daemon's own socket would deadlock; silently degrading
/// would return no hits even though the indexed fixture contains an exact lexical match.
#[cfg(feature = "code-search")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_keyword_search_reads_the_daemon_index_without_self_forwarding() {
    const HOSTED_CALL_BOUND: Duration = Duration::from_secs(20);

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    std::fs::write(
        repo.join("lib.rs"),
        b"pub fn parse_config(input: &str) -> usize { input.lines().count() }\n",
    )
    .expect("write fixture");
    std::fs::write(
        repo.join("basemind.toml"),
        b"\"$schema\" = \"v1\"\n\n[code_search]\nembed = false\n",
    )
    .expect("write embed-disabled config");

    let scan = Command::new(BIN)
        .current_dir(&repo)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .arg("scan")
        .output()
        .expect("scan hosted keyword fixture");
    assert!(
        scan.status.success(),
        "fixture scan must succeed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut observer = connect(&socket, "hosted-search-observer-before", &repo).await;
    let daemon_pid = observer.status().await.expect("status before hosted search").pid;
    drop(observer);
    let addr = http_frontend::await_http_ready(&generation_dir(&comms_dir), HTTP_READY)
        .await
        .expect("daemon-hosted MCP transport ready");
    let token =
        http_frontend::published_token(&generation_dir(&comms_dir)).expect("the daemon publishes its bearer token");

    let response = tokio::time::timeout(
        HOSTED_CALL_BOUND,
        hosted_tool_call(
            &addr,
            &token,
            &repo,
            "hosted-keyword-search",
            1,
            "code_search",
            json!({
                "query": "parse config",
                "lane": "keyword",
                "limit": 10
            }),
        ),
    )
    .await
    .expect("hosted keyword search must complete within the bound");
    assert!(
        response.get("error").is_none() && response["result"].is_object(),
        "hosted keyword search must return a successful JSON-RPC result: {response}"
    );
    assert_ne!(
        response["result"]["isError"],
        Value::Bool(true),
        "hosted keyword search returned a tool error: {response}"
    );
    let result_text = response["result"]["content"]
        .as_array()
        .and_then(|content| content.first())
        .and_then(|content| content["text"].as_str())
        .unwrap_or_else(|| panic!("hosted keyword result must contain JSON text: {response}"));
    let result: Value = serde_json::from_str(result_text)
        .unwrap_or_else(|error| panic!("hosted keyword tool payload is not JSON ({error}): {result_text}"));
    assert!(
        result.get("lanes_unavailable").is_none(),
        "hosted keyword search must not report an unavailable lane: {result}"
    );
    let hits = result["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hosted keyword response must contain hits: {result}"));
    assert!(
        !hits.is_empty(),
        "hosted keyword search must find the indexed fixture: {result}"
    );
    assert!(
        hits.iter().all(|hit| {
            hit["matched_lanes"]
                .as_array()
                .is_some_and(|lanes| lanes.iter().any(|lane| lane == "keyword"))
        }),
        "every keyword-only hit must identify the keyword lane: {result}"
    );

    let mut observer = connect(&socket, "hosted-search-observer-after", &repo).await;
    let after_pid = observer.status().await.expect("status after hosted search").pid;
    assert_eq!(after_pid, daemon_pid, "hosted search must stay on the original daemon");
    drop(observer);
    daemon.stop();
}

/// Regression: concurrent FIRST-touch rescans of the same COLD workspace must all succeed. The
/// daemon's workspace pool opens each cold store under fjall's exclusive index lock; before the open
/// was serialized, two rescans racing that cold open left the loser failing on the lock ("another
/// basemind process holds the lock") instead of sharing the winner's pooled entry — the post-open
/// reconciliation never ran because the loser failed inside `Store::open`. Two agents auto-scanning
/// the same repo at the same instant is exactly this race. Surfaced by the concurrency stress below.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_cold_rescans_open_the_workspace_once_and_all_succeed() {
    const RACERS: usize = 6;
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut clients = Vec::with_capacity(RACERS);
    for c in 0..RACERS {
        clients.push(connect(&socket, &format!("agent-race-{c}"), &repo).await);
    }
    let mut tasks = Vec::with_capacity(RACERS);
    for (c, mut client) in clients.into_iter().enumerate() {
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            client
                .rescan(repo, None, true, false)
                .await
                .map(|_| ())
                .map_err(|e| format!("racer {c}: {e}"))
        }));
    }
    for task in tasks {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(message)) => panic!("a cold-open racer must succeed, not fail on the lock: {message}"),
            Err(join) => panic!("a cold-open racer panicked: {join}"),
        }
    }
    daemon.stop();
}

/// Sustained multi-session stress against ONE daemon: many concurrent clients, each running its own
/// interleaved loop of full rescans + registry reads, all funneled through the daemon's
/// sole-writer workspace pool (the whole rearchitecture's promise — N sessions
/// on one repo all read AND write with no fjall lock downgrade). Asserts the daemon serves every
/// request with no torn index, no deadlock, and no panic, then stays responsive and consistent
/// afterward. `#[ignore]` (heavy); tune with `BASEMIND_STRESS_{CLIENTS,ITERS,FILES}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "stress: many concurrent clients hammer one daemon; run with --ignored"]
async fn stress_many_concurrent_sessions_read_and_write_through_one_daemon() {
    let clients = stress_knob("BASEMIND_STRESS_CLIENTS", 8);
    let iters = stress_knob("BASEMIND_STRESS_ITERS", 8);
    let files = stress_knob("BASEMIND_STRESS_FILES", 150);

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_bulk_git_repo(&repo, files);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut boot = connect(&socket, "agent-stress-boot", &repo).await;
    let repo_id = boot.list_workspaces().await.expect("list workspaces")[0]
        .repo_id
        .clone()
        .expect("git workspace has a repo id");
    drop(boot);

    let mut tasks = Vec::with_capacity(clients);
    for c in 0..clients {
        let socket = socket.clone();
        let repo = repo.clone();
        let repo_id = repo_id.clone();
        tasks.push(tokio::spawn(async move {
            let agent = format!("agent-stress-{c}");
            let mut client = connect(&socket, &agent, &repo).await;
            for _ in 0..iters {
                client
                    .rescan(repo.clone(), None, true, false)
                    .await
                    .map_err(|e| format!("{agent} rescan: {e}"))?;
                let workspaces = client
                    .list_workspaces()
                    .await
                    .map_err(|e| format!("{agent} list_workspaces: {e}"))?;
                if workspaces.is_empty() {
                    return Err(format!("{agent}: registry lost the workspace mid-storm"));
                }
                client
                    .list_worktrees(repo_id.clone())
                    .await
                    .map_err(|e| format!("{agent} list_worktrees: {e}"))?;
            }
            Ok::<(), String>(())
        }));
    }

    let join_all = async {
        let mut outcomes = Vec::with_capacity(tasks.len());
        for task in tasks {
            outcomes.push(task.await);
        }
        outcomes
    };
    let outcomes = tokio::time::timeout(Duration::from_secs(180), join_all)
        .await
        .expect("all stress clients must finish within 180s (no daemon deadlock)");
    for (i, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(message)) => panic!("stress client {i} failed: {message}"),
            Err(join) => panic!("stress client {i} panicked: {join}"),
        }
    }

    let mut after = connect(&socket, "agent-stress-after", &repo).await;
    let workspaces = after.list_workspaces().await.expect("post-storm list_workspaces");
    assert_eq!(
        workspaces.len(),
        1,
        "exactly one workspace must remain registered after the storm, got {}",
        workspaces.len()
    );
    let report = after
        .rescan(repo.clone(), None, true, false)
        .await
        .expect("post-storm rescan");
    assert!(
        report.scanned >= 1,
        "a post-storm rescan must still do real work (index not torn), got scanned={}",
        report.scanned
    );
    drop(after);
    daemon.stop();
}

/// Regression for the `comms stop` no-op (#34): a `Stop` RPC must actually terminate the daemon by
/// firing the accept-loop shutdown signal — not merely ack while the process lingers, which left
/// orphaned daemons piling up across sessions, reaped only by an external kill. This spawns a real
/// daemon, sends `basemind comms stop`, and asserts the process exits ON ITS OWN within a tight
/// window, WITHOUT the test ever killing it. Before the fix the broker held no shutdown sender, so
/// `begin_drain` set `Draining` but never broke the accept loop and this would time out.
#[test]
fn comms_stop_terminates_the_daemon_without_an_external_kill() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    std::fs::create_dir_all(&comms_dir).expect("mkdir comms");
    let socket = comms_socket_path(&generation_dir(&comms_dir));

    let mut child = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn comms daemon");

    // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
    let ready_by = Instant::now() + Duration::from_secs(180);
    while Instant::now() < ready_by && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon did not become ready");

    let stop = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output()
        .expect("run comms stop");
    assert!(
        stop.status.success(),
        "comms stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let exit_by = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < exit_by {
        if child.try_wait().expect("try_wait").is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon did not self-terminate after `comms stop` within 10s (the #34 no-op bug)");
    }
    let _ = child.wait();
    assert!(
        !probe_alive(&socket),
        "the socket must be released once the daemon self-terminates"
    );
}

/// Issue #44: SIGTERM must interrupt a MID-SCAN daemon. Every drain route (Stop RPC, SIGTERM,
/// idle-reap, socket-ownership loss) converges on `Broker::finish_drain`, which now trips the
/// broker's `ScanCancel` token so the in-flight `spawn_blocking` scan stops at per-file granularity,
/// and the daemon entry point bounds its runtime teardown (`RUNTIME_SHUTDOWN_TIMEOUT`) instead of
/// letting the implicit runtime drop wait forever on the blocking thread. Before the fix, the drain
/// grace elapsed, the accept loop exited — and the process then hung inside the runtime drop until
/// the scan of the whole tree completed, so only SIGKILL could end a scanning daemon; against this
/// fixture (a scan that outlasts the 12s exit deadline) the try_wait poll below timed out.
///
/// Consistency after the interrupt is the second half: blobs are content-addressed, fjall commits
/// per batch, and the cancelled pass skips the stale purge, so a fresh daemon on the same comms dir
/// must reopen the partially-committed index cleanly and complete a full rescan.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_mid_scan_exits_within_grace_and_index_reopens_cleanly() {
    // ~47 MB of generated Rust: a debug-build scan of this runs well past the 12s exit deadline ~keep
    // (~23s measured on an M4), so a pre-fix daemon — which cannot exit before the scan finishes — ~keep
    // reliably fails the deadline, while a post-fix daemon cancels and exits within a few seconds. ~keep
    const HEAVY_FILES: usize = 3000;
    const HEAVY_FNS_PER_FILE: usize = 200;

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    std::fs::create_dir_all(&comms_dir).expect("mkdir comms");
    let repo = tmp.path().join("repo");
    init_heavy_git_repo(&repo, HEAVY_FILES, HEAVY_FNS_PER_FILE);

    let socket = comms_socket_path(&generation_dir(&comms_dir));
    let mut child = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn comms daemon");
    // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
    let ready_by = Instant::now() + Duration::from_secs(180);
    while Instant::now() < ready_by && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon did not become ready");

    let mut client = connect(&socket, "agent-sigterm", &repo).await;
    let rescan_repo = repo.clone();
    let rescan_task = tokio::spawn(async move { client.rescan(rescan_repo, None, true, false).await });
    scan_in_flight(&comms_dir, &repo, || rescan_task.is_finished()).await;
    assert!(
        !rescan_task.is_finished(),
        "the scan finished before SIGTERM was sent, so nothing below would test a mid-scan signal"
    );

    let term = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("run kill -TERM");
    assert!(term.success(), "kill -TERM must be delivered");

    // ~keep The drain may wait out its grace for a link and then bound the runtime teardown
    // ~keep (`RUNTIME_SHUTDOWN_TIMEOUT`, 15 s); a fixed 12 s raced both under load. That a pre-fix
    // ~keep daemon ran the scan to the end instead is caught below by the scan's status.
    let exit_by = Instant::now() + basemind::comms::daemon::DRAIN_GRACE + Duration::from_secs(15 + 10);
    let mut exited = false;
    while Instant::now() < exit_by {
        if child.try_wait().expect("try_wait").is_some() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon did not exit within its drain bound of SIGTERM with a scan in flight (the SIGKILL-only hang)");
    }
    let _ = child.wait();

    // The in-flight rescan resolved rather than hanging: Ok if it somehow finished before the ~keep
    // drain, otherwise a clean Err (cancelled-pass error or broken link) — never a panic. ~keep
    let rescan_outcome = tokio::time::timeout(Duration::from_secs(15), rescan_task)
        .await
        .expect("the in-flight rescan future must resolve once the daemon exits, not hang");
    match rescan_outcome {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(join) => panic!("rescan task must not panic: {join}"),
    }
    let workspace = comms_dir
        .join("cache")
        .join("workspaces")
        .join(basemind::store::workspace_key(&repo));
    // ~keep A cancelled pass flushes what it committed under a `cancelled` health, so a missing
    // ~keep sidecar means the pass never reached its flush and proves nothing about the cancel.
    let status = basemind::store_layout::read_status_sidecar(&workspace);
    assert!(
        status.as_ref().is_some_and(|status| status.is_degraded()),
        "the scan was cancelled, not run to the end: {status:?}"
    );

    // A fresh daemon reopens the partially-committed index cleanly and completes the full pass. ~keep
    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut client = connect(&socket, "agent-sigterm-2", &repo).await;
    let report = client
        .rescan(repo.clone(), None, true, false)
        .await
        .expect("a full rescan after the interrupted pass must succeed");
    assert!(
        report.scanned >= HEAVY_FILES,
        "the interrupted index must reopen cleanly and the full rescan cover every file, got scanned={}",
        report.scanned
    );
    drop(client);
    daemon.stop();
}

/// The machine registry is a durable msgpack snapshot: it must survive the daemon exiting and a
/// fresh daemon reloading the same `BASEMIND_DATA_HOME`.
#[tokio::test(flavor = "multi_thread")]
async fn registry_survives_a_daemon_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut alice = connect(&socket, "agent-alice", &repo).await;
    let workspaces = alice.list_workspaces().await.expect("list workspaces");
    assert_eq!(workspaces.len(), 1, "Hello cwd auto-registers exactly one workspace");
    let repo_id = workspaces[0].repo_id.clone().expect("a git workspace has a repo id");

    drop(alice);
    daemon.stop();

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    // ~keep Bob connects from another directory: a Hello from `repo` would register it again and
    // the assertions below would hold with no snapshot at all.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("create a second directory");
    let mut bob = connect(&socket, "agent-bob", &elsewhere).await;
    let workspaces = bob.list_workspaces().await.expect("list workspaces after restart");
    assert!(
        workspaces
            .iter()
            .any(|workspace| workspace.repo_id.as_deref() == Some(repo_id.as_str())),
        "the workspace registered before the restart reloads from the snapshot: {workspaces:?}"
    );

    let worktrees = bob
        .list_worktrees(repo_id.clone())
        .await
        .expect("list worktrees after restart");
    assert!(
        worktrees.iter().any(|w| w.name == "(main)"),
        "(main) worktree present after restart"
    );

    drop(bob);
    daemon.stop();
}

/// Two `basemind comms daemon` processes racing a cold `comms_dir` converge on exactly one live
/// daemon: the bind-as-lock in `singleton::bind_listener` means the loser's bind fails
/// (`AddrInUse`), it probes the winner's socket, finds it alive, and exits cleanly rather than
/// unlinking and rebinding. Both children are reaped regardless of who won.
#[tokio::test(flavor = "multi_thread")]
async fn should_converge_on_one_live_daemon_when_two_processes_race_a_cold_bind() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let socket = comms_socket_path(&generation_dir(&comms_dir));

    let spawn_one = || {
        Command::new(BIN)
            .args(["comms", "daemon"])
            .env("BASEMIND_COMMS_DIR", &comms_dir)
            .env("BASEMIND_DATA_HOME", &comms_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon")
    };

    let mut child_a = spawn_one();
    let mut child_b = spawn_one();

    // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut alive = false;
    while Instant::now() < deadline {
        if probe_alive(&socket) {
            alive = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(alive, "one of the two racing daemons must come up serving");

    let mut client = connect(&socket, "agent-race", tmp.path()).await;
    let report = client.status().await.expect("status against the winning daemon");
    assert!(
        report.pid > 0,
        "the winning daemon reports a real pid, got {}",
        report.pid
    );
    drop(client);

    let _ = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output();

    for child in [&mut child_a, &mut child_b] {
        if child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(300));
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
        }
        let _ = child.wait();
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !probe_alive(&socket) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("socket still answers after both racing daemons were reaped");
}

/// The registry's branch and worktree rows track real git state through the daemon's lifetime:
/// creating a branch, adding a linked worktree, and removing it are all picked up on the NEXT
/// Hello (`populate_git` re-enumerates from git plumbing), without restarting the daemon itself.
#[tokio::test(flavor = "multi_thread")]
async fn should_reflect_branch_creation_and_worktree_add_remove_on_fresh_connects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut client = connect(&socket, "agent-reg", &repo).await;
    let workspaces = client.list_workspaces().await.expect("list workspaces");
    assert_eq!(workspaces.len(), 1, "Hello cwd auto-registers exactly one workspace");
    let repo_id = workspaces[0].repo_id.clone().expect("a git workspace has a repo id");

    let branches = client.list_branches(repo_id.clone()).await.expect("list branches");
    assert!(
        branches.iter().any(|b| b.name == "main"),
        "the initial checkout's branch is enumerated, got {branches:?}"
    );
    drop(client);

    git(&["branch", "feature"], &repo);
    let mut client = connect(&socket, "agent-reg-2", &repo).await;
    let branches = client
        .list_branches(repo_id.clone())
        .await
        .expect("list branches after branch create");
    assert!(
        branches.iter().any(|b| b.name == "feature"),
        "the new branch is enumerated after a fresh Hello re-populates, got {branches:?}"
    );
    drop(client);

    let linked_path = tmp.path().join("linked-wt");
    git(
        &[
            "worktree",
            "add",
            "-b",
            "wt-feature",
            linked_path.to_str().expect("utf8 path"),
            "feature",
        ],
        &repo,
    );
    let mut client = connect(&socket, "agent-reg-3", &repo).await;
    let worktrees = client
        .list_worktrees(repo_id.clone())
        .await
        .expect("list worktrees after add");
    let linked = worktrees
        .iter()
        .find(|w| w.path == linked_path.canonicalize().expect("canonicalize linked path"));
    assert!(
        linked.is_some(),
        "the newly linked worktree is enumerated after a fresh Hello, got {worktrees:?}"
    );
    assert_eq!(
        linked.expect("checked above").branch.as_deref(),
        Some("wt-feature"),
        "the linked worktree's checked-out branch is recorded"
    );
    drop(client);

    git(&["worktree", "remove", linked_path.to_str().expect("utf8 path")], &repo);
    let mut client = connect(&socket, "agent-reg-4", &repo).await;
    let worktrees = client
        .list_worktrees(repo_id.clone())
        .await
        .expect("list worktrees after remove");
    assert!(
        !worktrees
            .iter()
            .any(|w| w.path == linked_path.canonicalize().unwrap_or(linked_path.clone())),
        "the removed worktree is pruned from the registry after a fresh Hello, got {worktrees:?}"
    );
    drop(client);

    daemon.stop();
}

/// Guards the known orphan-daemon-pile-up bug: a daemon whose socket is unlinked out from under
/// it (e.g. reclaimed by a second daemon after a crash left a stale file) must notice via its
/// ownership watchdog and self-terminate, rather than lingering as an unreachable orphan.
///
/// Run explicitly: the watchdog fires on a ~30s timer (`OWNERSHIP_CHECK_EVERY` in
/// `src/cli/comms_daemon.rs`), so this test is inherently slow.
#[cfg(all(feature = "comms", unix))]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "watchdog fires on a ~30s timer; run explicitly with --ignored"]
async fn should_self_terminate_when_its_socket_is_reclaimed_by_another_daemon() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let socket = comms_socket_path(&generation_dir(&comms_dir));

    let mut child_a = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon A");

    // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon A must come up before we orphan it");

    std::fs::remove_file(&socket).expect("unlink daemon A's socket");

    let mut child_b = Command::new(BIN)
        .args(["comms", "daemon"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon B");

    // A cold daemon opens its fjall store (many F_FULLFSYNC calls) before it accepts, which stalls for tens of seconds when the disk is contended, so wait generously.
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline && !probe_alive(&socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(probe_alive(&socket), "daemon B must come up on the rebound socket");

    let watchdog_deadline = Instant::now() + Duration::from_secs(90);
    let mut a_exited = false;
    while Instant::now() < watchdog_deadline {
        if child_a.try_wait().ok().flatten().is_some() {
            a_exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        a_exited,
        "daemon A must self-terminate once its socket is reclaimed by daemon B (orphan watchdog)"
    );

    let _ = child_a.wait();
    let _ = Command::new(BIN)
        .args(["comms", "stop"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .output();
    if child_b.try_wait().ok().flatten().is_none() {
        std::thread::sleep(Duration::from_millis(300));
        if child_b.try_wait().ok().flatten().is_none() {
            let _ = child_b.kill();
        }
    }
    let _ = child_b.wait();
}

/// A pre-0.22 install left a legacy IN-REPO `.basemind/index.msgpack` (stale `schema_ver`) at the
/// old location. Since the global-cache re-root, the daemon's rescan never reads or writes that
/// file — all state lives under `BASEMIND_DATA_HOME/cache/workspaces/<key>/` — so a `rescan`
/// against such a repo must succeed, leave the legacy file untouched, and populate the global
/// cache for that workspace key.
#[tokio::test(flavor = "multi_thread")]
async fn should_rescan_successfully_and_ignore_a_stale_legacy_in_repo_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    #[derive(serde::Serialize)]
    struct LegacyIndex {
        schema_ver: u16,
        files: std::collections::BTreeMap<String, ()>,
        doc_files: std::collections::BTreeMap<String, ()>,
    }
    let legacy_dir = repo.join(".basemind");
    std::fs::create_dir_all(&legacy_dir).expect("mkdir legacy .basemind");
    let legacy_index = LegacyIndex {
        schema_ver: 21,
        files: std::collections::BTreeMap::new(),
        doc_files: std::collections::BTreeMap::new(),
    };
    let legacy_bytes = rmp_serde::to_vec_named(&legacy_index).expect("encode legacy index");
    let legacy_index_path = legacy_dir.join("index.msgpack");
    std::fs::write(&legacy_index_path, &legacy_bytes).expect("write legacy index.msgpack");
    let legacy_bytes_before = std::fs::read(&legacy_index_path).expect("read back legacy index.msgpack");

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut client = connect(&socket, "agent-migrate", &repo).await;
    let report = client
        .rescan(repo.clone(), None, true, false)
        .await
        .expect("rescan a repo carrying a stale legacy in-repo index");
    assert!(
        report.scanned >= 1,
        "rescan must actually scan the repo's files, got scanned={}",
        report.scanned
    );

    let legacy_bytes_after = std::fs::read(&legacy_index_path).expect("re-read legacy index.msgpack");
    assert_eq!(
        legacy_bytes_after, legacy_bytes_before,
        "the in-repo legacy .basemind/index.msgpack must be left byte-for-byte untouched"
    );

    // NOTE: we assert the workspace directory's existence (the weaker, currently-checkable
    let workspace_key = basemind::store::workspace_key(&repo);
    let workspace_dir = comms_dir.join("cache").join("workspaces").join(&workspace_key);
    assert!(
        workspace_dir.exists(),
        "the global cache must gain a workspace dir for this repo at {}",
        workspace_dir.display()
    );

    drop(client);
    daemon.stop();
}

/// A `comms stop` that lands while a forwarded rescan is in flight is refused with `daemon_busy`
/// (ADR-0024 rule 3: work in flight keeps the daemon), so the scan is never cancelled under its
/// client: the rescan finishes with real work, and the stop asked again afterwards is accepted. A
/// stop that lost the race to a scan already finished is simply accepted. A fresh daemon on the
/// same `comms_dir` then reloads the registry snapshot intact.
#[tokio::test(flavor = "multi_thread")]
async fn should_drain_cleanly_with_an_in_flight_rescan_and_reload_registry_after_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();

    let mut client = connect(&socket, "agent-drain", &repo).await;
    let workspaces = client.list_workspaces().await.expect("list workspaces");
    let repo_id = workspaces[0].repo_id.clone().expect("git workspace has a repo id");

    let rescan_repo = repo.clone();
    let rescan_task = tokio::spawn(async move { client.rescan(rescan_repo, None, true, false).await });
    scan_in_flight(&comms_dir, &repo, || rescan_task.is_finished()).await;

    let comms_dir_for_stop = comms_dir.clone();
    let stop_output = tokio::task::spawn_blocking(move || {
        Command::new(BIN)
            .args(["comms", "stop"])
            .env("BASEMIND_COMMS_DIR", &comms_dir_for_stop)
            .output()
    })
    .await
    .expect("join stop task")
    .expect("run the `comms stop` CLI invocation");
    let stop_stderr = String::from_utf8_lossy(&stop_output.stderr).into_owned();
    let refused = !stop_output.status.success();
    if refused {
        assert!(
            stop_stderr.contains("daemon_busy") && stop_stderr.contains("work=1"),
            "a stop refused mid-rescan must name the work in flight, stderr: {stop_stderr}"
        );
    }

    // ~keep A refused stop leaves the rescan to finish; a cold one under load outran 15 s.
    let rescan_outcome = tokio::time::timeout(Duration::from_secs(120), rescan_task)
        .await
        .expect("in-flight rescan must resolve within the timeout, not hang");
    match rescan_outcome {
        Ok(Ok(report)) => {
            assert!(
                report.scanned >= 1,
                "a rescan that completed must report real work, got scanned={}",
                report.scanned
            );
        }
        Ok(Err(client_error)) => {
            assert!(
                !refused,
                "a daemon that refused the stop must finish the rescan, got: {client_error}"
            );
        }
        Err(join_error) => panic!("rescan task must not panic, got: {join_error}"),
    }
    if refused {
        let comms_dir_for_stop = comms_dir.clone();
        let again = tokio::task::spawn_blocking(move || {
            Command::new(BIN)
                .args(["comms", "stop"])
                .env("BASEMIND_COMMS_DIR", &comms_dir_for_stop)
                .output()
        })
        .await
        .expect("join second stop task")
        .expect("run the second `comms stop`");
        assert!(
            again.status.success(),
            "the stop asked again after the rescan finished must be accepted, stderr: {}",
            String::from_utf8_lossy(&again.stderr)
        );
    }
    daemon.stop();

    let daemon = Daemon::start(&comms_dir);
    let socket = daemon.socket().to_path_buf();
    let mut client = connect(&socket, "agent-drain-2", &repo).await;
    let workspaces = client.list_workspaces().await.expect("list workspaces after restart");
    assert_eq!(
        workspaces.len(),
        1,
        "the registered workspace survives the drain + restart"
    );
    assert_eq!(
        workspaces[0].repo_id.as_deref(),
        Some(repo_id.as_str()),
        "the same repo id reloads from the snapshot after the drain"
    );

    drop(client);
    daemon.stop();
}

/// Run a `--json` CLI query in-process (no daemon exists yet, and none may be spawned), returning
/// the exact payload a writer session produces. This is the ground truth the daemon-backed paths
/// are held to.
fn writer_truth(repo: &Path, comms_dir: &Path, args: &[&str]) -> Value {
    let output = Command::new(BIN)
        .current_dir(repo)
        .env("BASEMIND_DATA_HOME", comms_dir)
        .env("BASEMIND_COMMS_DIR", comms_dir)
        .env("BASEMIND_NO_AUTOSPAWN", "1")
        .arg("--json")
        .args(args)
        .output()
        .expect("run writer-path query");
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{args:?} is not JSON: {error}"))
}

/// The hits of a `code_refs` payload as rows, each with its `path`: the tool lists them under their
/// file (`{"<path>": [hit, ...]}`).
fn hit_rows(payload: &Value) -> Vec<Value> {
    payload["hits"]
        .as_object()
        .unwrap_or_else(|| panic!("hits are grouped by file: {payload}"))
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

/// The tool payload inside a hosted JSON-RPC `tools/call` response.
fn hosted_payload(response: &Value) -> Value {
    let text = response["result"]["content"]
        .as_array()
        .and_then(|content| content.first())
        .and_then(|content| content["text"].as_str())
        .unwrap_or_else(|| panic!("hosted response has no text content: {response}"));
    serde_json::from_str(text).unwrap_or_else(|error| panic!("hosted payload is not JSON ({error}): {text}"))
}

/// Page through a writer-session `--json` CLI query via `--cursor`, returning the first page's total
/// and each page's hits. Run it before a daemon is up: with one up the CLI forwards to it.
fn writer_paginated(repo: &Path, comms_dir: &Path, args: &[&str]) -> (Value, Vec<Vec<Value>>) {
    let mut pages: Vec<Vec<Value>> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut total = Value::Null;
    for page in 0..200usize {
        let mut page_args: Vec<&str> = args.to_vec();
        if let Some(cursor) = &cursor {
            page_args.extend(["--cursor", cursor.as_str()]);
        }
        let payload = writer_truth(repo, comms_dir, &page_args);
        if page == 0 {
            total = payload["total"].clone();
        }
        pages.push(hit_rows(&payload));
        cursor = payload["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    (total, pages)
}

/// Page through a hosted `code_refs` query via `next_cursor`, returning the reported total and each
/// page's hits.
async fn hosted_paginated(addr: &str, token: &str, repo: &Path, tool_args: Value) -> (Value, Vec<Vec<Value>>) {
    const HOSTED_CALL_BOUND: Duration = Duration::from_secs(30);
    let mut pages: Vec<Vec<Value>> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut total = Value::Null;
    for id in 1..200usize {
        let mut arguments = tool_args.clone();
        if let Some(cursor) = &cursor {
            arguments["cursor"] = json!(cursor);
        }
        let response = tokio::time::timeout(
            HOSTED_CALL_BOUND,
            hosted_tool_call(addr, token, repo, "refs-hosted", id, "code_refs", arguments),
        )
        .await
        .expect("hosted call completes");
        let payload = hosted_payload(&response);
        if id == 1 {
            // `total` counts matches from the cursor onward, so only the first page reports the whole.
            total = payload["total"].clone();
        }
        assert_ne!(
            response["result"]["isError"],
            Value::Bool(true),
            "hosted code_refs returned a tool error: {response}"
        );
        assert!(
            payload.get("notice").is_none() || payload["notice"].is_null(),
            "a complete answer carries no truncation notice: {payload}"
        );
        pages.push(hit_rows(&payload));
        cursor = payload["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    (total, pages)
}

/// The bug this pins: a daemon-backed session used to answer `references` / `implementations` from
/// an in-RAM projection capped at `max_map_cache_mb`, so on a big corpus the result was a silently
/// truncated lower bound. Both daemon-backed shapes — a CLI call forwarded to the daemon and a
/// daemon-hosted connection — must now return exactly what a writer session returns, cursor
/// pagination included. A name scan runs in content-key order and only each page is sorted by path
/// (ADR-0036 decision 6), so a paginated answer is held page by page to the writer session paged
/// with the same arguments, never to the concatenation of one big page.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_backed_references_and_implementations_equal_the_writer_session() {
    const FILES: usize = 150;

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    for k in 0..FILES {
        let dir = repo.join(format!("pkg{}", k % 5));
        std::fs::create_dir_all(&dir).expect("mkdir pkg");
        std::fs::write(
            dir.join(format!("f{k}.rs")),
            format!(
                "pub trait Tr{t} {{ fn run(&self); }}\npub struct S{k};\n\
                 impl Tr{t} for S{k} {{ fn run(&self) {{ get_arg(); }} }}\n\
                 pub fn d{k}() {{ get_arg(); other_{o}(); }}\n",
                t = k % 3,
                o = k % 4,
            ),
        )
        .expect("write source");
    }
    std::fs::write(
        repo.join("basemind.toml"),
        b"\"$schema\" = \"v1\"\n\n[code_search]\nembed = false\n",
    )
    .expect("write embed-disabled config");
    let scan = Command::new(BIN)
        .current_dir(&repo)
        .env("BASEMIND_DATA_HOME", &comms_dir)
        .arg("scan")
        .output()
        .expect("scan fixture");
    assert!(
        scan.status.success(),
        "fixture scan must succeed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    // An explicit budget far above the page: the default one would cut the ground truth short.
    let truth_refs = writer_truth(
        &repo,
        &comms_dir,
        &[
            "code",
            "references",
            "get_arg",
            "--limit",
            "1000",
            "--max-tokens",
            "1000000",
        ],
    );
    let truth_impls = writer_truth(
        &repo,
        &comms_dir,
        &[
            "code",
            "implementations",
            "Tr1",
            "--limit",
            "1000",
            "--max-tokens",
            "1000000",
        ],
    );
    assert_eq!(
        truth_refs["total"],
        (FILES * 2) as u64,
        "ground truth sees every get_arg call: {truth_refs}"
    );
    assert_eq!(
        truth_impls["total"],
        (FILES / 3) as u64,
        "ground truth sees every Tr1 impl"
    );

    // The writer session paged exactly as the hosted calls below page: taken before the daemon is
    // up, since with one up the CLI forwards to it.
    let refs_args = ["code", "references", "get_arg", "--limit", "40"];
    let (truth_refs_total, truth_refs_pages) = writer_paginated(&repo, &comms_dir, &refs_args);
    let impls_args = ["code", "implementations", "Tr1", "--limit", "9"];
    let (truth_impls_total, truth_impls_pages) = writer_paginated(&repo, &comms_dir, &impls_args);
    assert_eq!(
        truth_refs_total, truth_refs["total"],
        "a paged writer session reports the whole total"
    );
    assert_eq!(
        truth_impls_total, truth_impls["total"],
        "a paged writer session reports the whole total"
    );
    assert!(
        truth_refs_pages.len() > 1 && truth_impls_pages.len() > 1,
        "the fixture spans several pages: {} / {}",
        truth_refs_pages.len(),
        truth_impls_pages.len()
    );
    for (pages, truth) in [(&truth_refs_pages, &truth_refs), (&truth_impls_pages, &truth_impls)] {
        let mut paged: Vec<Value> = pages.iter().flatten().cloned().collect();
        let mut whole = hit_rows(truth);
        let key = |row: &Value| (row["path"].to_string(), row["line"].as_u64(), row["column"].as_u64());
        paged.sort_by_key(key);
        whole.sort_by_key(key);
        assert_eq!(paged, whole, "the writer's pages together hold its single-page answer");
    }

    let daemon = Daemon::start(&comms_dir);

    // A CLI next to the daemon: the whole call is forwarded over a relay session.
    let forwarded = writer_truth(
        &repo,
        &comms_dir,
        &[
            "code",
            "references",
            "get_arg",
            "--limit",
            "1000",
            "--max-tokens",
            "1000000",
        ],
    );
    assert_eq!(forwarded["total"], truth_refs["total"], "{forwarded}");
    assert_eq!(
        hit_rows(&forwarded),
        hit_rows(&truth_refs),
        "a forwarded CLI call returns the writer's hits, in order"
    );

    // Daemon-hosted shape: the real MCP tool, paginated.
    let addr = http_frontend::await_http_ready(&generation_dir(&comms_dir), HTTP_READY)
        .await
        .expect("daemon-hosted MCP transport ready");
    let token =
        http_frontend::published_token(&generation_dir(&comms_dir)).expect("the daemon publishes its bearer token");
    await_settled(&addr, &token, &repo).await;
    let (total, pages) = hosted_paginated(&addr, &token, &repo, json!({"name": "get_arg", "limit": 40})).await;
    assert_eq!(
        total, truth_refs["total"],
        "hosted references total equals the writer's"
    );
    assert_eq!(
        pages, truth_refs_pages,
        "hosted references, paginated, equal the writer's pages, each in order"
    );
    let (total, pages) = hosted_paginated(
        &addr,
        &token,
        &repo,
        json!({"name": "Tr1", "relation": "implementations", "limit": 9}),
    )
    .await;
    assert_eq!(
        total, truth_impls["total"],
        "hosted implementations total equals the writer's"
    );
    assert_eq!(
        pages, truth_impls_pages,
        "hosted implementations, paginated, equal the writer's pages, each in order"
    );

    daemon.stop();
}

/// Two installed releases must not fight over one daemon. A release up to 0.33 keeps its daemon at
/// the top of the comms directory and stops, then kills, any daemon whose version or protocol is not
/// its own. Here a stand-in for that daemon holds the unscoped socket and reports 0.33.0 /
/// protocol 4, naming a real process as its pid. Bringing this binary's daemon up in the same comms
/// directory must leave it uncontacted and that process alive, with both daemons serving, and must
/// put nothing at the top of the comms directory or of the daemon registry but a generation
/// directory: those two top levels are what the released binary reads.
#[cfg(unix)]
#[test]
fn a_daemon_of_this_generation_starts_beside_a_pre_generation_daemon_and_leaves_it_alone() {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use basemind::comms::protocol::{CommsOut, CommsRequest, CommsResponse, PROTO_VER, StatusReport};
    use basemind::daemon_lock::pid_is_live;

    const RELEASED_VERSION: &str = "0.33.0";
    const RELEASED_PROTO: u32 = 4;

    /// Kills the stand-in process and stops this generation's daemon however the test ends.
    struct Cleanup<'a> {
        pid: u32,
        comms_dir: &'a Path,
        data_home: &'a Path,
    }
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = Command::new("kill").args(["-9", &self.pid.to_string()]).output();
            let _ = Command::new(BIN)
                .args(["comms", "stop"])
                .env("BASEMIND_COMMS_DIR", self.comms_dir)
                .env("BASEMIND_DATA_HOME", self.data_home)
                .output();
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn ask_status(socket: &Path) -> Option<StatusReport> {
        let mut stream = UnixStream::connect(socket).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let body = rmp_serde::to_vec_named(&CommsRequest::Status).ok()?;
        stream.write_all(&u32::try_from(body.len()).ok()?.to_be_bytes()).ok()?;
        stream.write_all(&body).ok()?;
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).ok()?;
        let mut reply = vec![0u8; u32::from_be_bytes(prefix) as usize];
        stream.read_exact(&mut reply).ok()?;
        match rmp_serde::from_slice::<CommsOut>(&reply).ok()? {
            CommsOut::Response(CommsResponse::Status(report)) => Some(report),
            _ => None,
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let data_home = tmp.path().join("data");
    std::fs::create_dir_all(&comms_dir).expect("mkdir comms");
    std::fs::create_dir_all(&data_home).expect("mkdir data home");

    // Started through a shell that exits at once, so it is reparented to init and a kill would ~keep
    // make it disappear rather than linger as this test's zombie, which still reads as live. ~keep
    let spawned = Command::new("sh")
        .args(["-c", "sleep 300 >/dev/null 2>&1 & echo $!"])
        .output()
        .expect("spawn the released daemon's stand-in process");
    let released_pid: u32 = String::from_utf8_lossy(&spawned.stdout)
        .trim()
        .parse()
        .expect("stand-in pid");
    let _cleanup = Cleanup {
        pid: released_pid,
        comms_dir: &comms_dir,
        data_home: &data_home,
    };
    assert!(pid_is_live(released_pid), "the stand-in process must be running");

    let released_socket = comms_dir.join("comms.sock");
    let listener = UnixListener::bind(&released_socket).expect("bind the pre-generation socket");
    listener.set_nonblocking(true).expect("nonblocking");
    let contacts = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (counted, stop) = (contacts.clone(), done.clone());
    let released_daemon = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            counted.fetch_add(1, Ordering::SeqCst);
            stream.set_nonblocking(false).expect("blocking");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut prefix = [0u8; 4];
            if stream.read_exact(&mut prefix).is_err() {
                continue;
            }
            let mut request = vec![0u8; u32::from_be_bytes(prefix) as usize];
            if stream.read_exact(&mut request).is_err() {
                continue;
            }
            let reply = rmp_serde::to_vec_named(&CommsOut::Response(CommsResponse::Status(StatusReport {
                pid: released_pid,
                version: RELEASED_VERSION.to_string(),
                build_id: String::new(),
                proto_ver: RELEASED_PROTO,
                uptime_secs: 60,
                agent_store: "open".to_string(),
            })))
            .expect("encode status");
            let _ = stream.write_all(&u32::try_from(reply.len()).expect("len fits").to_be_bytes());
            let _ = stream.write_all(&reply);
        }
    });

    let start = Command::new(BIN)
        .args(["comms", "start"])
        .env("BASEMIND_COMMS_DIR", &comms_dir)
        .env("BASEMIND_DATA_HOME", &data_home)
        .output()
        .expect("run comms start");
    assert!(
        start.status.success(),
        "comms start failed beside a pre-generation daemon: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let ours = comms_socket_path(&generation_dir(&comms_dir));
    assert_ne!(ours, released_socket, "the two generations must not share an endpoint");
    // `comms start` returns once the daemon answers `Ping`; `Status` waits for the store, whose ~keep
    // cold open stalls for tens of seconds on a contended disk. ~keep
    let ready_by = Instant::now() + Duration::from_secs(180);
    let ours_report = loop {
        if let Some(report) = ask_status(&ours) {
            break report;
        }
        assert!(
            Instant::now() < ready_by,
            "this generation's daemon must answer on its own socket"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(ours_report.proto_ver, PROTO_VER);
    assert_ne!(ours_report.pid, released_pid);

    assert_eq!(
        contacts.load(Ordering::SeqCst),
        0,
        "the pre-generation daemon must not be contacted at all"
    );
    assert!(
        pid_is_live(released_pid),
        "the pre-generation daemon's process must not be signalled"
    );
    let released_report = ask_status(&released_socket).expect("the pre-generation daemon still serves its socket");
    assert_eq!(released_report.version, RELEASED_VERSION);
    assert_eq!(released_report.proto_ver, RELEASED_PROTO);
    assert!(pid_is_live(ours_report.pid), "both daemons end up alive");

    let generation = format!("g{}", basemind::comms::singleton::RENDEZVOUS_GENERATION);
    assert_eq!(
        names_in(&comms_dir),
        ["comms.sock".to_string(), generation.clone()],
        "no lock, pidfile or store of this generation's daemon may land beside the pre-generation ones"
    );
    assert_eq!(
        names_in(&data_home.join("daemons")),
        [generation],
        "this generation's registry record must be out of a pre-generation scan's sight"
    );

    done.store(true, Ordering::SeqCst);
    released_daemon.join().expect("released daemon thread");
}

/// Wait until a hosted answer carries no lifecycle notice: the stack has warmed its map and finished
/// the catch-up pass it runs over a populated index.
async fn await_settled(addr: &str, token: &str, repo: &Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    for id in 1.. {
        let response = hosted_tool_call(
            addr,
            token,
            repo,
            "settle-reader",
            id,
            "code_symbols",
            json!({ "name": "x" }),
        )
        .await;
        if response["result"]["structuredContent"].get("notice").is_none() {
            return;
        }
        assert!(Instant::now() < deadline, "the hosted stack never settled: {response}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll `code_symbols {name}` through the daemon's hosted HTTP front-end until it lists `name`. Only
/// reads: nothing here asks for a rescan, so a hit means the daemon caught up on its own.
async fn await_symbol(comms_dir: &Path, repo: &Path, name: &str, bound: Duration) {
    let addr = http_frontend::await_http_ready(&generation_dir(comms_dir), HTTP_READY)
        .await
        .expect("daemon-hosted MCP transport ready");
    let token =
        http_frontend::published_token(&generation_dir(comms_dir)).expect("the daemon publishes its bearer token");
    let deadline = Instant::now() + bound;
    for id in 1.. {
        let response = hosted_tool_call(
            &addr,
            &token,
            repo,
            "catch-up-reader",
            id,
            "code_symbols",
            json!({ "name": name }),
        )
        .await;
        if response["result"]["structuredContent"]["total"] == json!(1) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "`{name}` was never indexed without a rescan call: {response}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A daemon that opens a workspace over a populated index catches up on what changed while no daemon
/// watched it (an idle exit, then a `git checkout` or an edit): a symbol added meanwhile is found
/// with no `index_rescan` call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_daemon_catches_up_on_edits_made_while_it_was_down() {
    const FOUND_BOUND: Duration = Duration::from_secs(120);

    let tmp = tempfile::tempdir().expect("tempdir");
    let comms_dir = tmp.path().join("comms");
    let repo = tmp.path().join("repo");
    init_git_repo(&repo);

    let daemon = Daemon::start(&comms_dir);
    await_symbol(&comms_dir, &repo, "alpha", FOUND_BOUND).await;
    daemon.stop();

    std::fs::write(repo.join("b.rs"), b"pub fn beta_added_while_down() {}\n").expect("write b.rs");

    let daemon = Daemon::start(&comms_dir);
    await_symbol(&comms_dir, &repo, "beta_added_while_down", FOUND_BOUND).await;
    daemon.stop();
}
