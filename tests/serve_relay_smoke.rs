//! `basemind serve` is a thin stdio↔daemon relay, not a server in its own right: it ensures the
//! comms daemon (the real rmcp host) and byte-pumps this process's stdin/stdout to the daemon's
//! relay socket. This is the transport every stdio MCP client uses (Codex, Gemini, Kimi, OpenCode,
//! Hermes, and the generic `basemind serve` registration); HTTP-native clients (Claude, Cursor)
//! dial the daemon URL instead and are covered by `http_frontend_smoke.rs`.
//!
//! This test drives the REAL relay end to end: it spawns the built `basemind` binary as a child
//! `serve` process over an `rmcp` `TokioChildProcess` transport — exactly how a stdio client
//! connects — and asserts the handshake, the tool listing, and a real tool round-trip all flow
//! through the relay to the daemon-hosted router. Hermetic via `init_isolated_cache`, so the daemon,
//! its socket, and the index live in a per-process tempdir and never touch the developer's daemon.
#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::Path;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command as AsyncCommand;

fn git(repo: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// A minimal indexed repo: one Rust file with a couple of symbols and a call site, enough to make
/// `list_tools` non-trivial and `outline` return real structure through the relay.
fn build_repo() -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\npub fn caller() { alpha(); alpha(); }\n",
    )
    .expect("write a.rs");
    dir
}

fn run_scan(root: &Path) {
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    // `#[tokio::test]`, so run the sync scan on a dedicated std thread to mirror the production context.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
                basemind::retrieval_wiring::backend(),
            )
            .expect("scan");
        });
    });
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
    basemind::wire::decode(&raw).unwrap_or(Value::Null)
}

fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_relays_stdio_client_to_the_daemon_hosted_router() {
    let dir = build_repo();
    let root = dir.path();
    run_scan(root);

    // Spawn the built binary as a real stdio client would: `basemind --root <repo> serve`. The child
    // inherits this process's isolated BASEMIND_DATA_HOME / BASEMIND_COMMS_DIR (set by
    // init_isolated_cache), so its ensure_daemon converges on the same hermetic daemon.
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    let service = ().serve(transport).await.expect("rmcp handshake over the relay");

    // The handshake carried the daemon-hosted server's own instructions — proof we reached basemind's
    // router through the relay, not some empty shim.
    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    // Matched on the stable identity phrase rather than a quoted sentence: the instructions are prose
    // that gets re-tuned (they were rewritten to fit the client's 2048-char ceiling), and quoting a
    // phrase that later gets edited away turns this into a false failure about relaying.
    assert!(
        instructions.contains("basemind") && instructions.contains("indexed context layer"),
        "relayed server should carry basemind's instructions: {instructions:?}"
    );

    // tools/list flows through the relay and returns the full tool surface. Asserted on tool names
    // rather than a count, so a change to the size of the surface does not read as a broken relay.
    let tools = service.list_all_tools().await.expect("list_tools over relay");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for tool in ["code_outline", "git_log", "memory_search", "index_status"] {
        assert!(
            names.contains(&tool),
            "relayed tool listing should include the always-on `{tool}` tool: {names:?}"
        );
    }

    // A real tool round-trip: outline the scanned file and confirm the symbols came back through the
    // relay from the daemon-hosted index.
    let outlined = service
        .call_tool(call_params("code_outline", serde_json::json!({ "path": "a.rs" })))
        .await
        .expect("code_outline call over relay");
    let body = decode_text(&outlined);
    let symbols = body
        .get("symbols")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let names: Vec<&str> = symbols
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        names.contains(&"alpha") && names.contains(&"caller"),
        "outline over the relay should list the scanned symbols, got {names:?}"
    );

    // A second client must not be able to recycle the shared daemon out from under this live relay.
    // This is the multi-agent failure that surfaced as Codex's permanent `Transport closed`: one
    // session installed a new build and stopped the singleton, severing every other stdio host.
    let stop = AsyncCommand::new(bin)
        .arg("comms")
        .arg("stop")
        .output()
        .await
        .expect("request daemon stop while relay is live");
    assert!(
        !stop.status.success(),
        "stop must be refused while another relay is live"
    );
    let stderr = String::from_utf8_lossy(&stop.stderr);
    assert!(
        stderr.contains("daemon_busy"),
        "refusal should expose the stable daemon_busy code: {stderr}"
    );

    let after_refusal = service
        .call_tool(call_params("code_outline", serde_json::json!({ "path": "a.rs" })))
        .await
        .expect("relay should remain callable after a refused stop");
    assert_eq!(decode_text(&after_refusal)["path"], "a.rs");

    service.cancel().await.expect("shut down the relay client");
}

/// Live `serve` sessions of `root`'s identity, as `basemind agent status` reports them.
async fn live_serve_sessions(root: &Path) -> u64 {
    let status = AsyncCommand::new(env!("CARGO_BIN_EXE_basemind"))
        .arg("--root")
        .arg(root)
        .arg("--json")
        .args(["agent", "status"])
        .output()
        .await
        .expect("run `agent status`");
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let report: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    report["store"]["sessions"]
        .as_u64()
        .unwrap_or_else(|| panic!("no session count: {report}"))
}

/// The CLI-only commands, which ADR-0018 and ADR-0047 keep off the agent surface.
const CLI_ONLY: [(&str, &str); 11] = [
    ("memory_delete", r#"{"key":"k"}"#),
    ("git_churn", "{}"),
    ("git_by_path", r#"{"pattern":"a"}"#),
    ("code_chunk", r#"{"path":"a.rs"}"#),
    ("code_impact", r#"{"path":"a.rs"}"#),
    ("code_cycles", "{}"),
    ("overview", "{}"),
    ("git_hotspots", "{}"),
    ("code_migration_check", "{}"),
    ("code_explain", r#"{"target":"a.rs"}"#),
    ("code_deadcode", "{}"),
];

/// A relay session is an agent's unless its hello says it is a CLI command's. An agent session (a
/// `serve` relay) answers the CLI-only commands as unknown tools; a CLI session lists the same
/// tools byte for byte, answers a CLI-only command, and reports a failed call as a JSON-RPC error
/// with the code the CLI exits by.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_cli_session_reaches_the_cli_only_commands() {
    let dir = build_repo();
    let root = dir.path();
    git(root, &["add", "-A"]);
    git(
        root,
        &["-c", "user.name=t", "-c", "user.email=t@e.x", "commit", "-qm", "init"],
    );
    run_scan(root);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let agent = ().serve(TokioChildProcess::new(cmd).expect("spawn basemind serve")).await.expect("serve relay");
    for (name, args) in CLI_ONLY {
        let error = agent
            .call_tool(call_params(name, serde_json::from_str(args).expect("args")))
            .await
            .expect_err("an agent session does not know a CLI-only command");
        assert!(error.to_string().contains("tool not found"), "{name}: {error}");
    }

    let paths = basemind::comms::singleton::resolve_paths().expect("comms paths");
    let mut stream = basemind::comms::relay_connect::connect_stream(&paths.socket_path)
        .await
        .expect("connect to the daemon the serve relay brought up");
    let hello = basemind::comms::relay::RelayHello {
        relay_proto_ver: basemind::comms::relay::RELAY_PROTO_VER,
        root: root.canonicalize().expect("canonical root"),
        view: basemind::store::VIEW_WORKING.to_string(),
        principal: basemind::comms::v2::identity::resolve_for_root(root)
            .expect("the serve session's identity")
            .principal,
        host: None,
        notices: Vec::new(),
        client: basemind::comms::relay::RelayClient::Cli,
    };
    let welcome = basemind::comms::relay::client_handshake(&mut stream, &hello)
        .await
        .expect("relay handshake");
    assert!(welcome.accepted && welcome.cli, "{welcome:?}");
    let cli = ().serve(tokio::io::split(stream)).await.expect("CLI session");
    assert_eq!(
        live_serve_sessions(root).await,
        1,
        "a CLI command's session is not a serve session of the same identity"
    );

    let mut tools: Vec<_> = cli
        .list_all_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .filter(|tool| !basemind::mcp::agent_tools::AGENT_TOOLS.contains(&tool.name.as_ref()))
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut listed = serde_json::to_string_pretty(&tools).expect("serialize tools");
    listed.push('\n');
    let committed = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tools_list.json"))
        .expect("tools/list snapshot")
        .replace("\r\n", "\n");
    assert!(listed == committed, "a CLI session lists other tools:\n{listed}");

    let churn = cli
        .call_tool(call_params("git_churn", serde_json::json!({})))
        .await
        .expect("a CLI session runs `git churn`");
    assert_ne!(churn.is_error, Some(true), "{churn:?}");
    assert!(decode_text(&churn).get("files").is_some(), "{churn:?}");
    let cycles = cli
        .call_tool(call_params("code_cycles", serde_json::json!({})))
        .await
        .expect("a CLI session runs `code cycles`");
    assert_ne!(cycles.is_error, Some(true), "{cycles:?}");
    assert!(decode_text(&cycles).get("coverage").is_some(), "{cycles:?}");

    let overview = cli
        .call_tool(call_params("overview", serde_json::json!({ "sections": "languages" })))
        .await
        .expect("a CLI session runs `overview`");
    assert_ne!(overview.is_error, Some(true), "{overview:?}");
    assert!(decode_text(&overview).get("languages").is_some(), "{overview:?}");

    let no_rules = cli
        .call_tool(call_params("code_migration_check", serde_json::json!({})))
        .await
        .expect_err("a repository without rule files has no migration to check");
    assert!(no_rules.to_string().contains("no rule file"), "{no_rules}");

    let refused = cli
        .call_tool(call_params("code_outline", serde_json::json!({ "path": "missing.rs" })))
        .await
        .expect_err("a failed call is a JSON-RPC error on a CLI session");
    let rmcp::ServiceError::McpError(refused) = refused else {
        panic!("not a JSON-RPC error: {refused}");
    };
    assert_eq!(refused.code, rmcp::model::ErrorCode::INVALID_PARAMS, "{refused:?}");
    assert!(refused.message.contains("missing.rs"), "{refused:?}");

    cli.cancel().await.expect("close the CLI session");
    agent.cancel().await.expect("close the serve relay");
}
