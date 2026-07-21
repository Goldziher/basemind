//! Controlled smoke: run the real agent engine end-to-end against a *scripted* model — no network,
//! no API key, fully deterministic. This drives the same `Session::run` loop the TUI uses, with the
//! real tool registry (code-nav against the in-process basemind index + shell), so it exercises the
//! integration the unit tests mock out.
//!
//! The model's turns come from a [`Scenario`](basemind_agent::replay::Scenario) — a fixed sequence
//! of assistant replies (text and/or tool calls), so the run is repeatable. A scenario can be
//! supplied as a JSON file; otherwise a built-in default drives one code-map query plus one
//! permission-gated shell command.
//!
//! Usage:
//!   cargo run -p basemind-agent --example scripted [-- <scenario.json>] [repo-root]
//!
//! Scenario JSON shape:
//!   {
//!     "system": "optional system prompt",
//!     "user": "the user message that starts the turn",
//!     "turns": [
//!       { "text": "assistant text", "tools": [ { "id": "c1", "name": "code:search_symbols",
//!                                                 "args": { "needle": "run_turn" } } ] },
//!       { "text": "closing line" }
//!     ]
//!   }
//!
//! Permissions are auto-approved (this is a non-interactive smoke). Read-only code-nav tools
//! auto-allow via the base ruleset; the shell command triggers a permission request that is granted.

use std::path::PathBuf;
use std::sync::Arc;

use basemind_agent::replay::{Scenario, ScriptToolCall, ScriptTurn};
use basemind_agent::tools::{ShellTool, code_nav_tools, git_history_tools};
use basemind_agent::{
    AgentClient, AgentCommand, AgentEvent, PermissionDecision, Session, ToolRegistry, in_proc_channel,
};

/// Model-step budget for the scripted turn.
const MAX_STEPS: u32 = 20;

/// The built-in scenario: exercise a real code-map query, then a permission-gated shell command,
/// then stop. Chosen so the run touches streaming text, a read-only auto-allowed tool, a suspend →
/// approve → exec round-trip, the multi-round loop, and a clean stop.
fn default_scenario() -> Scenario {
    Scenario {
        system: Some("You are a coding assistant inside basemind. Prefer code-map tools over reading files.".into()),
        user: "Where is run_turn defined, and confirm the shell works?".into(),
        turns: vec![
            ScriptTurn {
                text: Some("Looking up run_turn. ".into()),
                tools: vec![ScriptToolCall {
                    id: "call_1".into(),
                    name: "code:search_symbols".into(),
                    args: serde_json::json!({ "needle": "run_turn" }),
                }],
            },
            ScriptTurn {
                text: Some("Now checking the shell. ".into()),
                tools: vec![ScriptToolCall {
                    id: "call_2".into(),
                    name: "shell:exec".into(),
                    args: serde_json::json!({ "command": "echo smoke-ok" }),
                }],
            },
            ScriptTurn {
                text: Some("Both worked.".into()),
                tools: Vec::new(),
            },
        ],
    }
}

#[tokio::main]
async fn main() {
    let (scenario_path, root) = parse_args();
    let scenario = match scenario_path {
        Some(path) => match Scenario::load(&path) {
            Ok(scenario) => scenario,
            Err(error) => {
                eprintln!("scenario {}: {error}", path.display());
                std::process::exit(2);
            }
        },
        None => default_scenario(),
    };

    // Real tools: attach the in-process code map when the repo is scanned, else run shell-only.
    let mut tools = ToolRegistry::new();
    let server = match basemind::cli::context::build_server(&root, "working", Default::default()) {
        Ok(server) => {
            tools.register_all(code_nav_tools());
            tools.register_all(git_history_tools());
            Some(Arc::new(server))
        }
        Err(error) => {
            eprintln!("(code map unavailable — run `basemind scan` for code-nav tools: {error})");
            None
        }
    };
    tools.register(Arc::new(ShellTool));

    let session = Session::with_provider(
        scenario.provider(),
        root,
        server,
        tools,
        scenario.system.clone(),
        MAX_STEPS,
    );

    let (endpoint, mut client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    client
        .send_command(AgentCommand::UserMessage {
            text: scenario.user.clone(),
        })
        .await
        .expect("send prompt");

    let mut failures = 0u32;
    while let Some(event) = client.next_event().await {
        match event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::ToolStarted { name, args, .. } => println!("\n[tool] {name} {args}"),
            AgentEvent::ToolResult { ok, summary, .. } => {
                if !ok {
                    failures += 1;
                }
                println!("[tool result ok={ok}] {summary}");
            }
            AgentEvent::PermissionRequested {
                req_id,
                tool,
                action,
                target,
                ..
            } => {
                println!("\n[auto-approving {tool}: {action} {target}]");
                client
                    .send_command(AgentCommand::PermissionDecision {
                        req_id,
                        decision: PermissionDecision::AllowForSession,
                    })
                    .await
                    .expect("send decision");
            }
            AgentEvent::TurnFinished { reason, steps, .. } => {
                println!("\n[turn finished: {reason:?} in {steps} steps]");
                break;
            }
            AgentEvent::Error { message, fatal, .. } => {
                eprintln!("\n[error fatal={fatal}] {message}");
                if fatal {
                    failures += 1;
                    break;
                }
            }
            _ => {}
        }
    }

    let _ = client.send_command(AgentCommand::Shutdown).await;
    let _ = engine.await;

    if failures > 0 {
        eprintln!("\nsmoke FAILED: {failures} tool/error failure(s)");
        std::process::exit(1);
    }
    println!("\nsmoke OK");
}

/// Parse `[scenario.json] [repo-root]`. Either argument may be omitted; a `.json` argument is the
/// scenario, anything else is the repo root (default `.`).
fn parse_args() -> (Option<PathBuf>, PathBuf) {
    let mut scenario = None;
    let mut root = None;
    for arg in std::env::args().skip(1) {
        if arg.ends_with(".json") {
            scenario = Some(PathBuf::from(arg));
        } else {
            root = Some(PathBuf::from(arg));
        }
    }
    (scenario, root.unwrap_or_else(|| PathBuf::from(".")))
}
