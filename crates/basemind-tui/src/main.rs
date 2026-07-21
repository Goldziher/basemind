//! `basemind-tui` — a ratatui terminal front-end for the `basemind-agent` engine.
//!
//! The binary wires an in-process [`Session`] to the UI over the [`AgentClient`] boundary, then
//! runs the event loop in [`run`]. All engine interaction flows through commands and events; the
//! UI never touches engine internals.
//!
//! Usage: `basemind-tui ["<initial prompt>"] [--root <path>] [--resume <id> | --continue]`. Every
//! turn is persisted to a JSONL session log; `--continue` resumes this repo's latest session and
//! `--resume <id>` resumes a named one. The model is read from `BASEMIND_AGENT_MODEL` (default
//! `anthropic/claude-sonnet-4`) and the API key from `ANTHROPIC_API_KEY`.
//!
//! Built with `--features replay`, `--replay <scenario.json>` instead drives the whole UI against a
//! scripted model (no network, no API key) — the deterministic path used for smoke tests.

mod app;
mod config;
#[cfg(test)]
mod e2e;
mod markdown;
mod run;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use basemind::mcp::BasemindServer;
use basemind_agent::tools::{ShellTool, code_nav_tools, git_history_tools};
use basemind_agent::{AgentClient, AgentCommand, Session, SessionStore, ToolRegistry, in_proc_channel};

use crate::app::App;
use crate::config::{default_model_name, load_agent_config};

/// The system prompt handed to the session.
const SYSTEM_PROMPT: &str = "You are a coding assistant operating inside the basemind agent. Prefer \
    the code:outline and code:search_symbols tools over reading whole files. Be concise.";

/// How the session log should be opened.
enum Resume {
    /// Start a fresh session.
    Fresh,
    /// Resume this repo's latest session (falling back to fresh if there is none).
    Latest,
    /// Resume a named session by id.
    Id(String),
}

/// Command-line arguments: an optional initial prompt, a repo root, a resume mode, and an optional
/// scripted-replay scenario.
struct Args {
    prompt: Option<String>,
    root: PathBuf,
    resume: Resume,
    replay: Option<PathBuf>,
}

/// Parse `[prompt] [--root <path>] [--resume <id> | --continue] [--replay <scenario.json>]`.
fn parse_args() -> Args {
    let mut prompt = None;
    let mut root = PathBuf::from(".");
    let mut resume = Resume::Fresh;
    let mut replay = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                if let Some(value) = args.next() {
                    root = PathBuf::from(value);
                }
            }
            "--resume" => {
                if let Some(value) = args.next() {
                    resume = Resume::Id(value);
                }
            }
            "--continue" => resume = Resume::Latest,
            "--replay" => {
                if let Some(value) = args.next() {
                    replay = Some(PathBuf::from(value));
                }
            }
            _ if prompt.is_none() => prompt = Some(arg),
            _ => {}
        }
    }
    Args {
        prompt,
        root,
        resume,
        replay,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();

    // Attach the in-process code map if the repo is scanned; otherwise run shell-only. Print the ~keep
    // note BEFORE entering the alternate screen so it is visible. Shared by both session paths. ~keep
    let mut tools = ToolRegistry::new();
    let server = match basemind::cli::context::build_server(&args.root, "working", Default::default()) {
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

    // A scripted replay session (deterministic, no network) or a live-provider session. ~keep
    let (session, model, initial_prompt) = match args.replay.clone() {
        Some(path) => build_replay_session(path, args.root.clone(), server, tools)?,
        None => build_live_session(&args, server, tools).await?,
    };

    let (endpoint, client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    if let Some(prompt) = initial_prompt {
        client
            .send_command(AgentCommand::UserMessage { text: prompt })
            .await
            .context("send initial prompt")?;
    }

    let result = run::run(client, App::new(model)).await;

    // Let the engine drain its Shutdown before we surface any UI error. ~keep
    let _ = engine.await;
    result
}

/// Build a session backed by live providers, resuming or creating a persisted JSONL session log. On
/// resume the engine history is seeded; the App transcript is not re-hydrated in this slice.
async fn build_live_session(
    args: &Args,
    server: Option<Arc<BasemindServer>>,
    tools: ToolRegistry,
) -> Result<(Session, String, Option<String>)> {
    let config = load_agent_config(&args.root).context("load agent config")?;
    let model = default_model_name(&config).to_string();

    let (store, seed) = match &args.resume {
        Resume::Id(id) => {
            let (store, messages, meta) = SessionStore::open(&args.root, id).await.context("resume session")?;
            (store, Some((messages, meta)))
        }
        Resume::Latest => match SessionStore::latest_id(&args.root)
            .await
            .context("find latest session")?
        {
            Some(id) => {
                let (store, messages, meta) = SessionStore::open(&args.root, &id)
                    .await
                    .context("resume latest session")?;
                (store, Some((messages, meta)))
            }
            None => (SessionStore::create(&args.root).await.context("create session")?, None),
        },
        Resume::Fresh => (SessionStore::create(&args.root).await.context("create session")?, None),
    };

    let mut session = Session::new(&config, args.root.clone(), server, tools, Some(SYSTEM_PROMPT.into()))
        .context("start agent session")?;
    if let Some((messages, meta)) = seed {
        session = session.seed(messages, meta.input_tokens, meta.output_tokens);
    }
    Ok((session.persist_to(store), model, args.prompt.clone()))
}

/// Model-step budget for a scripted replay turn.
#[cfg(feature = "replay")]
const REPLAY_MAX_STEPS: u32 = 20;

/// Build a session that replays a scripted scenario (deterministic, no network / no API key). The
/// scenario's user message is returned as the auto-sent initial prompt.
#[cfg(feature = "replay")]
fn build_replay_session(
    path: PathBuf,
    root: PathBuf,
    server: Option<Arc<BasemindServer>>,
    tools: ToolRegistry,
) -> Result<(Session, String, Option<String>)> {
    use basemind_agent::replay::Scenario;

    let scenario = Scenario::load(&path).with_context(|| format!("load replay scenario {}", path.display()))?;
    let system = scenario.system.clone().unwrap_or_else(|| SYSTEM_PROMPT.into());
    let session = Session::with_provider(scenario.provider(), root, server, tools, Some(system), REPLAY_MAX_STEPS);
    Ok((session, "mock/scripted".into(), Some(scenario.user)))
}

/// Without the `replay` feature the scripted seam is compiled out, so `--replay` is an error.
#[cfg(not(feature = "replay"))]
fn build_replay_session(
    _path: PathBuf,
    _root: PathBuf,
    _server: Option<Arc<BasemindServer>>,
    _tools: ToolRegistry,
) -> Result<(Session, String, Option<String>)> {
    anyhow::bail!("--replay requires building basemind-tui with `--features replay`")
}
