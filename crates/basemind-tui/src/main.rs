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
//! By default the engine runs in-process. Two flags switch to a daemon-hosted engine over a
//! per-workspace Unix socket (`basemind-agent-ipc`):
//! - `--daemon` runs the engine headless (no UI), hosting one long-lived session and serving attaches.
//! - `--attach` runs the UI against that daemon; if none is running it transparently spawns a detached
//!   one first. Every attach joins the *same* session, so the session outlives any single UI and a
//!   later attach reconnects to a turn already in flight.
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
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use basemind::mcp::BasemindServer;
use basemind_agent::tools::{ShellTool, code_nav_tools, git_history_tools};
use basemind_agent::{AgentClient, AgentCommand, Session, SessionStore, ToolRegistry, in_proc_channel};
use basemind_agent_ipc::{
    UdsAgentClient, agent_socket_path, bind_listener, ensure_daemon, probe_alive, serve, spawn_detached,
};

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

/// Where the engine runs relative to this process.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Engine runs in this process, driving the UI directly (the default).
    InProc,
    /// Engine runs headless in this process, serving attaches over the socket (no UI).
    Daemon,
    /// UI runs in this process against a daemon-hosted engine, spawning one if none is running.
    Attach,
}

/// Command-line arguments: an optional initial prompt, a repo root, a resume mode, an optional
/// scripted-replay scenario, and how the engine is hosted.
struct Args {
    prompt: Option<String>,
    root: PathBuf,
    resume: Resume,
    replay: Option<PathBuf>,
    mode: Mode,
}

/// Parse `[prompt] [--root <path>] [--resume <id> | --continue] [--replay <scenario.json>]
/// [--daemon | --attach]`.
fn parse_args() -> Args {
    let mut prompt = None;
    let mut root = PathBuf::from(".");
    let mut resume = Resume::Fresh;
    let mut replay = None;
    let mut mode = Mode::InProc;
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
            "--daemon" => mode = Mode::Daemon,
            "--attach" => mode = Mode::Attach,
            _ if prompt.is_none() => prompt = Some(arg),
            _ => {}
        }
    }
    Args {
        prompt,
        root,
        resume,
        replay,
        mode,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    match args.mode {
        Mode::InProc => run_in_proc(args).await,
        Mode::Daemon => run_daemon(args).await,
        Mode::Attach => run_attach(args).await,
    }
}

/// Build the engine: the code-map tools (if the repo is scanned) plus a scripted-replay or
/// live-provider [`Session`]. Returns the session, the model name for the status bar, and the
/// initial prompt (if any). Shared by the in-process and daemon paths.
async fn build_engine(args: &Args) -> Result<(Session, String, Option<String>)> {
    // Attach the in-process code map if the repo is scanned; otherwise run shell-only. The note is ~keep
    // printed before any alternate screen is entered so it stays visible. ~keep
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

    match args.replay.clone() {
        Some(path) => build_replay_session(path, args.root.clone(), server, tools),
        None => build_live_session(args, server, tools).await,
    }
}

/// Run the engine in this process and drive the UI directly (the default mode).
async fn run_in_proc(args: Args) -> Result<()> {
    let (session, model, initial_prompt) = build_engine(&args).await?;

    let (endpoint, client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    let mut app = App::new(model);
    if let Some(prompt) = initial_prompt {
        // Mirror the auto-sent opening prompt into the transcript so it renders a `you:` line, the
        // same as a prompt typed at the input box (which records itself through `App::on_key`). ~keep
        app.push_user(prompt.clone());
        client
            .send_command(AgentCommand::UserMessage { text: prompt })
            .await
            .context("send initial prompt")?;
    }

    let result = run::run(client, app).await;

    // Let the engine drain its Shutdown before we surface any UI error. ~keep
    let _ = engine.await;
    result
}

/// Run the engine headless, hosting one long-lived session and serving attaches over the
/// per-workspace socket. No UI; runs until the process is killed. The initial prompt is ignored —
/// prompts arrive from attaches so the session stays shared.
async fn run_daemon(args: Args) -> Result<()> {
    let (session, _model, _initial_prompt) = build_engine(&args).await?;

    let socket_path = agent_socket_path(&args.root);
    let listener = bind_listener(&socket_path, probe_alive).context("bind agent daemon socket")?;
    eprintln!("agent daemon listening on {}", socket_path.display());

    let (endpoint, template) = in_proc_channel(32, 256);
    tokio::spawn(session.run(endpoint));

    // The template client is held here for the process's lifetime, so the engine's command channel
    // never closes between connections and the session persists across attaches. ~keep
    serve(listener, move || template.new_client())
        .await
        .context("serve agent daemon")?;
    Ok(())
}

/// Run the UI against a daemon-hosted engine, spawning a detached daemon first if none is running,
/// then joining its shared session.
async fn run_attach(args: Args) -> Result<()> {
    let socket_path = agent_socket_path(&args.root);
    ensure_daemon(&socket_path, || spawn_detached(daemon_command(&args)))
        .await
        .context("ensure an agent daemon is running")?;
    let client = UdsAgentClient::connect(&socket_path)
        .await
        .context("connect to the agent daemon")?;

    let model = attach_model_name(&args)?;
    let mut app = App::new(model);
    if let Some(prompt) = args.prompt.clone() {
        // Seed the transcript with the auto-sent prompt (a `you:` line) and drive it into the shared
        // daemon session. ~keep
        app.push_user(prompt.clone());
        client
            .send_command(AgentCommand::UserMessage { text: prompt })
            .await
            .context("send initial prompt")?;
    }
    run::run(client, app).await
}

/// The command that spawns a detached daemon for this repo: the current binary in `--daemon` mode,
/// carrying the same root, replay, and resume selection — but never the prompt (attaches own prompts).
fn daemon_command(args: &Args) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("basemind-tui"));
    let mut command = Command::new(exe);
    command.arg("--daemon").arg("--root").arg(&args.root);
    if let Some(replay) = &args.replay {
        command.arg("--replay").arg(replay);
    }
    match &args.resume {
        Resume::Id(id) => {
            command.arg("--resume").arg(id);
        }
        Resume::Latest => {
            command.arg("--continue");
        }
        Resume::Fresh => {}
    }
    command
}

/// The model name to show in the status bar for an attach, matching what the daemon runs: the
/// scripted model under `--replay`, else the configured default role's model.
fn attach_model_name(args: &Args) -> Result<String> {
    if args.replay.is_some() {
        return Ok("mock/scripted".into());
    }
    let config = load_agent_config(&args.root).context("load agent config")?;
    Ok(default_model_name(&config).to_string())
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
