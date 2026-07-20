//! In-process CLI that gives `basemind` 1:1 parity with the MCP tool surface.
//!
//! Every tool subcommand builds the matching `*Params` struct, constructs a
//! one-shot [`crate::mcp::BasemindServer`] (no background facilities), and calls
//! the identical `#[tool]` method an MCP client would dispatch — then renders the
//! returned [`rmcp::model::CallToolResult`]. Parity is by construction: the CLI
//! runs the same tool code, not a re-implementation.
//!
//! Layout:
//! - `context.rs` — one-shot server construction.
//! - `render.rs` — JSON extraction + generic human renderer.
//! - `codemap.rs` / `git.rs` / `memory.rs` / `web.rs` / `admin.rs` — subcommand groups.

pub mod admin;
pub mod codemap;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod comms;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod comms_daemon;
pub mod context;
pub mod git;
pub mod governance;
pub mod init;
pub mod init_rules;
pub mod memory;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod registry;
pub mod render;
#[cfg(all(feature = "shells", any(unix, windows)))]
pub mod shells;
pub mod web;

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Subcommand;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::cli::render::Emit;
use crate::config::DocumentsCliOverrides;

/// Tool subcommand groups dispatched through the in-process server.
#[derive(Subcommand, Debug)]
pub enum ToolCmd {
    /// Code-map queries (outline, search, references, call-graph, …).
    #[command(subcommand)]
    Query(codemap::QueryCmd),
    /// Git history / blame / diff queries.
    #[command(subcommand)]
    Git(git::GitCmd),
    /// Shared agent memory + document search (needs `--features memory,documents`).
    #[command(subcommand)]
    Memory(memory::MemoryCmd),
    /// Governance: mine co-change proposals, list, accept, reject (needs `--features memory`).
    #[command(subcommand)]
    Governance(governance::GovernanceCmd),
    /// On-demand web ingestion (needs `--features crawl`).
    #[command(subcommand)]
    Web(web::WebCmd),
    /// Headless agent shells: spawn / send / capture / kill / broadcast / list
    /// (needs `--features shells`).
    #[cfg(all(feature = "shells", any(unix, windows)))]
    #[command(subcommand)]
    Shells(shells::ShellsCmd),
    /// Aggregate telemetry into a usage summary.
    Telemetry {
        /// Aggregation window: `today` (default), `1h`, `24h`, `all`.
        #[arg(long)]
        window: Option<String>,
        /// Optional exact tool-name filter.
        #[arg(long)]
        tool: Option<String>,
    },
}

/// Map a tool `Result<CallToolResult, McpError>` into an `anyhow::Result`,
/// surfacing the tool's own error message verbatim. Tools that return an
/// `is_error` result still produce `Ok(...)` — the JSON payload describes the
/// condition, so we render it rather than fail the process.
pub fn run_tool(tool: &str, result: Result<CallToolResult, McpError>) -> Result<CallToolResult> {
    result.map_err(|e| anyhow::anyhow!("{tool}: {e}"))
}

/// Dispatch a tool subcommand group. Builds one one-shot server per invocation
/// (reused across the single call) and discovers the git repo + config the same
/// way `serve` does.
///
/// `process_started` is captured at `main()` entry. Everything from there up to the tool call —
/// clap parsing, root discovery, the tokio runtime build, the read-only store open, config load,
/// git-cache open — is charged to [`Emit::startup_us`], NOT to the tool's own `elapsed_us`. That
/// split is the point: it separates the per-process cost the CLI pays (and a long-lived `serve`
/// does not) from the cost of the query itself.
///
/// `cache` commands are dispatched separately by the caller via
/// [`admin::run_cache`] because they are the offline path and need no server.
pub fn run(
    root: &Path,
    view: &str,
    documents: DocumentsCliOverrides,
    json: bool,
    process_started: Instant,
    cmd: ToolCmd,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        let server = context::build_server(root, view, documents)?;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let opts = Emit {
            json,
            startup_us: u64::try_from(process_started.elapsed().as_micros()).unwrap_or(u64::MAX),
        };
        match cmd {
            ToolCmd::Query(q) => codemap::run(&server, q, &opts, &mut out).await?,
            ToolCmd::Git(g) => git::run(&server, g, &opts, &mut out).await?,
            ToolCmd::Memory(m) => memory::run(&server, m, &opts, &mut out).await?,
            ToolCmd::Governance(g) => governance::run(&server, g, &opts, &mut out).await?,
            ToolCmd::Web(w) => web::run(&server, w, &opts, &mut out).await?,
            #[cfg(all(feature = "shells", any(unix, windows)))]
            ToolCmd::Shells(s) => shells::run(&server, s, &opts, &mut out).await?,
            ToolCmd::Telemetry { window, tool } => admin::run_telemetry(&server, window, tool, &opts, &mut out).await?,
        }
        out.flush().context("flush stdout")?;
        Ok(())
    })
}

/// Dispatch the offline `cache` command group (no server / flock needed).
pub fn run_cache(root: &Path, cmd: admin::CacheCmd, json: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    admin::run_cache(root, cmd, json, &mut out)?;
    out.flush().context("flush stdout")?;
    Ok(())
}
