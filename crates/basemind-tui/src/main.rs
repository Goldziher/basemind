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

mod app;
mod config;
mod markdown;
mod run;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
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

/// Command-line arguments: an optional initial prompt, a repo root, and a resume mode.
struct Args {
    prompt: Option<String>,
    root: PathBuf,
    resume: Resume,
}

/// Parse `[prompt] [--root <path>] [--resume <id> | --continue]` from the process arguments.
fn parse_args() -> Args {
    let mut prompt = None;
    let mut root = PathBuf::from(".");
    let mut resume = Resume::Fresh;
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
            _ if prompt.is_none() => prompt = Some(arg),
            _ => {}
        }
    }
    Args { prompt, root, resume }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    let config = load_agent_config(&args.root).context("load agent config")?;
    let model = default_model_name(&config).to_string();

    // Attach the in-process code map if the repo is scanned; otherwise run shell-only. Print the
    // note BEFORE entering the alternate screen so it is visible.
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

    // Open the session log: resume a named or the latest session, else start fresh. On resume we
    // seed the engine history (the App transcript is NOT re-hydrated in this slice — resuming the
    // engine context is enough).
    let (store, seed) = match args.resume {
        Resume::Id(id) => {
            let (store, messages, meta) = SessionStore::open(&args.root, &id).await.context("resume session")?;
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

    let mut session =
        Session::new(&config, args.root, server, tools, Some(SYSTEM_PROMPT.into())).context("start agent session")?;
    if let Some((messages, meta)) = seed {
        session = session.seed(messages, meta.input_tokens, meta.output_tokens);
    }
    let session = session.persist_to(store);

    let (endpoint, client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    // Seed the first turn if an initial prompt was given.
    if let Some(prompt) = args.prompt {
        client
            .send_command(AgentCommand::UserMessage { text: prompt })
            .await
            .context("send initial prompt")?;
    }

    let result = run::run(client, App::new(model)).await;

    // Let the engine drain its Shutdown before we surface any UI error.
    let _ = engine.await;
    result
}
