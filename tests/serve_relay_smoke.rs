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
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command as AsyncCommand;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
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
    serde_json::from_str(&raw).unwrap_or(Value::Null)
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

    // tools/list flows through the relay and returns the full code-map surface.
    let tools = service.list_all_tools().await.expect("list_tools over relay");
    assert!(
        tools.len() >= 20,
        "relay should expose the full tool surface, got {}",
        tools.len()
    );
    assert!(
        tools.iter().any(|t| t.name == "outline"),
        "relayed tool listing should include `outline`"
    );

    // A real tool round-trip: outline the scanned file and confirm the symbols came back through the
    // relay from the daemon-hosted index.
    let outlined = service
        .call_tool(call_params("outline", serde_json::json!({ "path": "a.rs" })))
        .await
        .expect("outline call over relay");
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

    service.cancel().await.expect("shut down the relay client");
}
