//! Headless smoke: run one real agent turn against a live provider and print events to stdout.
//!
//! This is the M0 capstone — the engine actually talking to a model, with no TUI. It needs a real
//! API key in the environment (default: `ANTHROPIC_API_KEY`). Code-nav tools work only if the
//! workspace has been scanned (`basemind scan`); otherwise they report the code map as unavailable
//! and the shell tool still works.
//!
//! Usage:
//!   ANTHROPIC_API_KEY=... cargo run -p basemind-agent --example headless -- "<prompt>" [repo-root]
//!
//! Permissions are auto-approved for the session (this is a non-interactive smoke).

use std::path::PathBuf;
use std::sync::Arc;

use basemind::config::{ApiKey, LlmConfig};
use basemind_agent::config::{AgentConfig, RoleModels};
use basemind_agent::tools::{ShellTool, code_nav_tools};
use basemind_agent::{AgentCommand, AgentEvent, PermissionDecision, Session, ToolRegistry, in_proc_channel};

const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4";
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const SYSTEM_PROMPT: &str = "You are a coding assistant operating inside the basemind agent. Prefer \
    the code:outline and code:search_symbols tools over reading whole files. Be concise.";

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let prompt = args.next().unwrap_or_else(|| {
        eprintln!("usage: headless \"<prompt>\" [repo-root]");
        std::process::exit(2);
    });
    let root = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

    // A single default role; the key is read from the environment (BYO key). ~keep
    let config = AgentConfig {
        roles: RoleModels {
            default: LlmConfig {
                model: DEFAULT_MODEL.into(),
                api_key: ApiKey::Env {
                    env: API_KEY_ENV.into(),
                },
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    // Try to attach the in-process code map; fall back to shell-only if the repo isn't scanned. ~keep
    let mut tools = ToolRegistry::new();
    let server = match basemind::cli::context::build_server(&root, "working", Default::default()) {
        Ok(server) => {
            let server = Arc::new(server);
            tools.register_all(code_nav_tools());
            Some(server)
        }
        Err(error) => {
            eprintln!("(code map unavailable — run `basemind scan` for code-nav tools: {error})");
            None
        }
    };
    tools.register(Arc::new(ShellTool));

    let session = match Session::new(&config, root, server, tools, Some(SYSTEM_PROMPT.into())) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("failed to start session: {error}");
            std::process::exit(1);
        }
    };

    let (endpoint, mut client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    client
        .send_command(AgentCommand::UserMessage { text: prompt })
        .await
        .expect("send prompt");

    use basemind_agent::AgentClient;
    while let Some(event) = client.next_event().await {
        match event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::ToolStarted { name, args, .. } => println!("\n[tool] {name} {args}"),
            AgentEvent::ToolResult { ok, summary, .. } => println!("[tool result ok={ok}] {summary}"),
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
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                println!("\n[usage in={input_tokens} out={output_tokens}]");
            }
            AgentEvent::TurnFinished { reason, steps, .. } => {
                println!("\n[turn finished: {reason:?} in {steps} steps]");
                break;
            }
            AgentEvent::Error { message, fatal, .. } => {
                eprintln!("\n[error fatal={fatal}] {message}");
                if fatal {
                    break;
                }
            }
            _ => {}
        }
    }

    let _ = client.send_command(AgentCommand::Shutdown).await;
    let _ = engine.await;
}
