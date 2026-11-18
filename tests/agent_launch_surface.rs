//! Where `agent_launch` / `agent_stop` can be reached (ADR-0040, review 9's wiring conditions): only
//! from the stdio of a `basemind serve --lead` whose operator policy enables them. The daemon's relay
//! sessions (a plain `serve` and a CLI's), a lead without a policy, a lead whose repository carries
//! the policy in `basemind.toml`, and a lead inside a launched session answer them as unknown tools.
//! The HTTP front-end's half is in `http_frontend_smoke.rs`.
//!
//! Every child runs with the isolated data and comms dirs of `init_isolated_cache` and a temp `HOME`
//! / `XDG_CONFIG_HOME`, so no real user-level policy is read. The only harness launched is a
//! `/bin/sh` stand-in that reads its input until it is stopped.
#![cfg(all(feature = "comms", unix))]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tokio::process::Command as AsyncCommand;

const LEAD_TOOLS: [&str; 2] = ["agent_launch", "agent_stop"];

struct Layout {
    _dir: tempfile::TempDir,
    base: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    worktrees: PathBuf,
}

impl Layout {
    fn new() -> Self {
        basemind::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical");
        let repo = base.join("repo");
        for sub in ["repo", "home", "xdg", "worktrees/one"] {
            std::fs::create_dir_all(base.join(sub)).expect("mkdir");
        }
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .expect("git");
        assert!(status.success());
        std::fs::write(repo.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
        let stand_in = base.join("claude-stand-in");
        std::fs::write(&stand_in, "#!/bin/sh\ncat > /dev/null\n").expect("stand-in");
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self {
            home: base.join("home"),
            xdg: base.join("xdg"),
            worktrees: base.join("worktrees"),
            repo,
            _dir: dir,
            base,
        }
    }

    /// Where `directories` puts basemind's user-level configuration under this layout's home.
    fn config_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/basemind")
        } else {
            self.xdg.join("basemind")
        }
    }

    fn write_policy(&self) {
        let dir = self.config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod dir");
        let path = dir.join("agents.toml");
        std::fs::write(
            &path,
            format!(
                "[agents.launch]\nenabled = true\nworktrees_root = \"{}\"\nwall_time_secs = 60\n\
                 [agents.launch.harness.claude]\nexecutable = \"{}\"\nstop_grace_secs = 1\n",
                self.worktrees.display(),
                self.base.join("claude-stand-in").display()
            ),
        )
        .expect("policy");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod policy");
    }

    async fn serve(&self, lead: bool, env: &[(&str, &str)]) -> RunningService<RoleClient, ()> {
        let cmd = AsyncCommand::new(env!("CARGO_BIN_EXE_basemind")).configure(|c| {
            c.arg("--root").arg(&self.repo).arg("serve");
            if lead {
                c.arg("--lead");
            }
            c.env("HOME", &self.home)
                .env("XDG_CONFIG_HOME", &self.xdg)
                .env_remove("BASEMIND_AGENT_DEPTH")
                .env_remove("BASEMIND_LAUNCH_TOKEN")
                .envs(env.iter().copied());
        });
        ().serve(TokioChildProcess::new(cmd).expect("spawn serve"))
            .await
            .expect("handshake")
    }
}

fn params(name: &'static str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name).with_arguments(args.as_object().cloned().unwrap_or_default())
}

fn text(result: &CallToolResult) -> Value {
    let raw = result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

/// The session lists neither lead tool and answers both as unknown tools.
async fn lead_tools_unknown(session: &RunningService<RoleClient, ()>, who: &str) {
    let tools = session.list_all_tools().await.expect("tools/list");
    assert!(
        tools.iter().any(|tool| tool.name == "code_outline"),
        "{who}: a real tool list"
    );
    for name in LEAD_TOOLS {
        assert!(!tools.iter().any(|tool| tool.name == name), "{who} lists {name}");
        let error = session
            .call_tool(params(name, json!({ "session": 1 })))
            .await
            .expect_err("unknown tool");
        assert!(error.to_string().contains("tool not found"), "{who}: {name}: {error}");
    }
}

fn instructions(session: &RunningService<RoleClient, ()>) -> String {
    session
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default()
}

/// Review 9 condition 1: the daemon answers neither tool, for a plain `serve` relay session or a
/// CLI's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn neither_a_plain_serve_nor_a_cli_relay_session_reaches_the_lead_tools() {
    let layout = Layout::new();
    let plain = layout.serve(false, &[]).await;
    lead_tools_unknown(&plain, "a plain serve").await;
    assert!(!instructions(&plain).contains("agent_launch"));

    let paths = basemind::comms::singleton::resolve_paths().expect("comms paths");
    let mut stream = basemind::comms::relay_connect::connect_stream(&paths.socket_path)
        .await
        .expect("connect to the daemon");
    let hello = basemind::comms::relay::RelayHello {
        relay_proto_ver: basemind::comms::relay::RELAY_PROTO_VER,
        root: layout.repo.clone(),
        view: basemind::store::VIEW_WORKING.to_string(),
        principal: basemind::comms::v2::Principal::parse("cli-test").expect("principal"),
        host: None,
        notices: Vec::new(),
        client: basemind::comms::relay::RelayClient::Cli,
    };
    let welcome = basemind::comms::relay::client_handshake(&mut stream, &hello)
        .await
        .expect("relay handshake");
    assert!(welcome.accepted && welcome.cli, "{welcome:?}");
    let cli = ().serve(tokio::io::split(stream)).await.expect("CLI session");
    lead_tools_unknown(&cli, "a CLI relay session").await;
    cli.cancel().await.expect("close the CLI session");
    plain.cancel().await.expect("close the plain serve");
}

/// Off by default, and the repository cannot turn it on: with no operator file a lead lists no
/// lead tool, says why in its instructions, and an `[agents.launch]` table in the repository's
/// `basemind.toml` changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lead_without_an_operator_policy_lists_no_lead_tool_and_says_why() {
    let layout = Layout::new();
    std::fs::write(
        layout.repo.join("basemind.toml"),
        format!(
            "\"$schema\" = \"v1\"\n[agents.launch]\nenabled = true\nworktrees_root = \"{}\"\n\
             [agents.launch.harness.claude]\nexecutable = \"{}\"\n",
            layout.worktrees.display(),
            layout.base.join("claude-stand-in").display()
        ),
    )
    .expect("repo config");
    let lead = layout.serve(true, &[]).await;
    let said = instructions(&lead);
    assert!(said.contains("agent launching is off"), "{said}");
    assert!(
        said.contains("agents.toml"),
        "the notice names the operator's file: {said}"
    );
    lead_tools_unknown(&lead, "a lead without a policy").await;
    lead.cancel().await.expect("close");
}

/// Review 9 condition 6: a lead started inside a launched session hosts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lead_inside_a_launched_session_hosts_no_supervisor() {
    let layout = Layout::new();
    layout.write_policy();
    let nested = layout.serve(true, &[("BASEMIND_AGENT_DEPTH", "1")]).await;
    let said = instructions(&nested);
    assert!(said.contains("agent launching is off"), "{said}");
    assert!(said.contains("inside a launched session"), "{said}");
    lead_tools_unknown(&nested, "a nested lead").await;
    nested.cancel().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lead_with_an_operator_policy_launches_into_a_worktree_and_stops() {
    let layout = Layout::new();
    layout.write_policy();
    let lead = layout.serve(true, &[]).await;
    let said = instructions(&lead);
    assert!(said.contains("agent_launch / agent_stop are enabled"), "{said}");
    let tools = lead.list_all_tools().await.expect("tools/list");
    for name in LEAD_TOOLS {
        assert!(tools.iter().any(|tool| tool.name == name), "the lead lists {name}");
    }
    assert!(
        tools.iter().any(|tool| tool.name == "code_outline"),
        "and the daemon's tools"
    );

    let refused = lead
        .call_tool(params(
            "agent_launch",
            json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one", "executable": "/bin/sh" }),
        ))
        .await
        .expect("a refusal is a result");
    assert_eq!(refused.is_error, Some(true), "{refused:?}");

    let launched = lead
        .call_tool(params(
            "agent_launch",
            json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one" }),
        ))
        .await
        .expect("agent_launch");
    assert_ne!(launched.is_error, Some(true), "{launched:?}");
    let launched = text(&launched);
    assert!(
        launched["agent"]
            .as_str()
            .is_some_and(|agent| agent.starts_with("launched-claude-")),
        "{launched}"
    );
    assert!(
        launched["thread"].is_string(),
        "a thread was opened to the agent: {launched}"
    );
    let session = launched["session"].as_u64().expect("session id");

    let stopped = lead
        .call_tool(params("agent_stop", json!({ "session": session })))
        .await
        .expect("agent_stop");
    assert_ne!(stopped.is_error, Some(true), "{stopped:?}");

    let audit_dir = basemind::store::cache_root().join("agents");
    let logs: Vec<_> = std::fs::read_dir(&audit_dir).expect("audit dir").flatten().collect();
    assert!(
        !logs.is_empty(),
        "the lead keeps an audit log in {}",
        audit_dir.display()
    );
    lead.cancel().await.expect("close");
}

/// Review 9 condition 2: nothing in the lead opens a listener. The supervisor is reached only on the
/// lead's stdio; a source change that adds a socket has to face this test and review 9 condition 8.
#[test]
fn the_lead_code_opens_no_listener() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("src/serve_lead.rs")];
    for entry in std::fs::read_dir(root.join("src/lead")).expect("src/lead").flatten() {
        files.push(entry.path());
    }
    assert!(files.len() >= 4, "{files:?}");
    for file in files {
        let source = std::fs::read_to_string(&file).expect("read");
        for listener in ["Listener", ".bind(", "listen("] {
            assert!(!source.contains(listener), "{} contains {listener}", file.display());
        }
    }
}

/// Review 9 condition 3: a lead is non-dumpable by the time it answers, so no process of the user can
/// open its stdin through `/proc` and write the lead's requests into it. A plain `serve` is the
/// control: its `/proc` entries are the user's.
#[cfg(target_os = "linux")]
#[test]
fn a_lead_is_sealed_before_it_answers_and_a_plain_serve_is_not() {
    use std::io::{BufRead as _, Write as _};
    use std::os::unix::fs::MetadataExt as _;

    let layout = Layout::new();
    let sealed = |lead: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_basemind"));
        command.arg("--root").arg(&layout.repo).arg("serve");
        if lead {
            command.arg("--lead");
        }
        let mut child = command
            .env("HOME", &layout.home)
            .env("XDG_CONFIG_HOME", &layout.xdg)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn serve");
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"t","version":"0"}}}}}}"#
        )
        .expect("write initialize");
        let mut answer = String::new();
        std::io::BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut answer)
            .expect("read the answer");
        assert!(answer.contains("\"id\":1"), "{answer}");
        let pid = child.id();
        let owner = std::fs::metadata(format!("/proc/{pid}")).expect("proc entry").uid();
        // Readable whatever the seal: a `serve` started below the lead finds the marker here.
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).expect("comm");
        let writable = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/{pid}/fd/0"))
            .is_ok();
        drop(stdin);
        let _ = child.wait();
        (owner == 0, writable, comm.trim_end() == basemind_agents::HOST_MARKER)
    };
    assert_eq!(sealed(false), (false, true, false), "the control: a plain serve");
    assert_eq!(
        sealed(true),
        (true, false, true),
        "a lead, sealed and named from its main thread"
    );
}
