//! One identity for `serve`, the CLI and the hooks (ADR-0021 rule 1 and 2): a thread a `serve`
//! session creates is the CLI's own thread when the CLI runs, as the hooks do, from a
//! subdirectory of the checkout with the same `BASEMIND_HOST`. Hermetic via `init_isolated_cache`:
//! the daemon and every child share the per-process tempdir.
#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tokio::process::Command as AsyncCommand;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");
/// A host whose hook rewrites the agent tools' input, so `agent hook name` names the session.
const HOST: &str = "claude";

/// Run `basemind agent <args>` from `cwd` as the hooks do, returning its JSON output.
fn agent_cli(cwd: &Path, args: &[&str], stdin: Option<&str>) -> Value {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .arg("--json")
        .arg("agent")
        .args(args)
        .current_dir(cwd)
        .env("BASEMIND_HOST", HOST)
        .env_remove("BASEMIND_AGENT_ID")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn basemind agent");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.unwrap_or_default().as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("basemind agent");
    assert!(
        out.status.success(),
        "basemind agent {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_in_a_subdirectory_acts_as_the_serve_session() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .expect("git")
            .success()
    );
    let sub = root.join("deep").join("er");
    std::fs::create_dir_all(&sub).expect("subdirectory");

    let cmd = AsyncCommand::new(BIN).configure(|c| {
        c.arg("--root")
            .arg(root)
            .arg("serve")
            .env("BASEMIND_HOST", HOST)
            .env_remove("BASEMIND_AGENT_ID");
    });
    let service = ().serve(TokioChildProcess::new(cmd).expect("spawn serve")).await.expect("handshake");
    let params = CallToolRequestParams::new("agent_post").with_arguments(
        json!({"subject": "identity", "body": "from serve"})
            .as_object()
            .expect("object")
            .clone(),
    );
    let posted = service.call_tool(params).await.expect("agent_post");
    assert_ne!(posted.is_error, Some(true));
    let text = posted
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("a reply");
    let thread = serde_json::from_str::<Value>(&text).expect("JSON")["thread"]
        .as_str()
        .expect("thread")
        .to_string();

    let status = agent_cli(&sub, &["status"], None);
    let principal = status["principal"].as_str().expect("principal").to_string();
    assert!(principal.starts_with(&format!("{HOST}-")), "{status}");

    let listed = agent_cli(&sub, &["threads"], None);
    let mine = listed["threads"]
        .as_array()
        .expect("threads")
        .iter()
        .find(|t| t["id"] == thread.as_str())
        .unwrap_or_else(|| panic!("the serve session's thread is the CLI's own: {listed}"))
        .clone();
    assert_eq!(mine["creator"], principal.as_str(), "{mine}");
    assert_eq!(mine["member"], true);

    let name = agent_cli(&sub, &["hook", "name"], Some(r#"{"session_id":"s-1"}"#));
    let name = name.as_str().expect("a name").to_string();
    assert!(name.starts_with("session-"), "{name}");
    let announced = agent_cli(&sub, &["inbox", "--announce", "--as", &name], None);
    assert_eq!(
        announced["messages"].as_array().map(Vec::len),
        Some(1),
        "the session's own thread is announced to its hook-named sub-identity: {announced}"
    );

    service.cancel().await.expect("close serve");
}
