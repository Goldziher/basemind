//! In-process CLI over the same tool bodies the MCP server runs.
//!
//! Every MCP tool has a subcommand that builds the tool's `*Params` struct, constructs a one-shot
//! [`crate::mcp::BasemindServer`] (no background facilities), and calls the `*_call` method the
//! `#[tool]` shim calls, then renders the returned [`rmcp::model::CallToolResult`]. Parity is by
//! construction: the CLI runs the same tool code, not a re-implementation. The CLI additionally
//! has shorthands and administrative commands with no MCP tool (ADR-0018).
//!
//! Layout:
//! - `context.rs` — one-shot server construction, or the daemon session a call is forwarded over.
//! - `forward.rs` — the relay session that forwards a call to the daemon when it is up.
//! - `render.rs` — JSON extraction + generic human renderer.
//! - `code.rs` / `git.rs` / `memory.rs` / `admin.rs` — subcommand groups.

pub mod admin;
pub mod agent;
pub mod analysis;
mod analysis_migration;
pub mod choices;
pub mod code;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod comms_daemon;
pub mod context;
pub mod env_config;
pub mod exit;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod forward;
pub mod git;
pub mod hook;
pub mod init;
pub mod init_gitignore;
pub mod init_rules;
pub mod init_settings;
pub mod init_verb;
pub mod interrupt;
pub mod manual;
pub mod memory;
pub mod overview;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod registry;
pub mod render;
pub mod rescan;
pub mod sessions;

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Subcommand;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::cli::render::Emit;
use crate::config::overrides::DocumentsCliOverrides;

/// Tool subcommand groups dispatched through the in-process server.
#[derive(Subcommand, Debug)]
pub enum ToolCmd {
    /// Code lookups: outline, symbols, read, grep, search, refs, find (plus shorthands).
    #[command(subcommand)]
    Code(code::CodeCmd),
    /// Git history / blame / diff queries.
    #[command(subcommand)]
    Git(git::GitCmd),
    /// Shared agent memory and document search (needs `--features memory,documents`).
    #[command(subcommand)]
    Memory(memory::MemoryCmd),
    /// Administration: telemetry, tokens, eval.
    #[command(subcommand)]
    Admin(admin::AdminCmd),
    /// Index health for this workspace: file counts, languages, scan age.
    Status,
    /// The repository's shape: languages, layout, manifests, entry points, largest and most
    /// imported files, routes, naming conventions, recent commits.
    Overview(overview::OverviewArgs),
}

/// Resolve a user-supplied CLI path into the repo-relative `RelPath` key the index is keyed by
/// (scanner-produced: no leading `./`, never absolute). Every path argument of every tool subcommand
/// goes through this, so `/abs/repo/src/foo.rs`, `./src/foo.rs` and `src/foo.rs` are the same file.
///
/// Relative inputs are taken relative to the repository root (not the shell's cwd). Paths that
/// escape or fall outside the repository can't match an indexed file, so the raw input is kept and
/// the downstream tool reports "file not indexed" rather than silently mangling it.
pub(crate) fn resolve_path(tools: &context::Tools, path: &str) -> crate::path::RelPath {
    match crate::path::normalize_query_path(path, tools.root()) {
        Some(rel) => crate::path::RelPath::from(rel),
        None => crate::path::RelPath::from(path),
    }
}

/// Map a tool `Result<CallToolResult, McpError>` into an `anyhow::Result`,
/// surfacing the tool's own error message verbatim. Tools that return an
/// `is_error` result still produce `Ok(...)` — the JSON payload describes the
/// condition, so we render it rather than fail the process.
///
/// Two failures are classified for the exit-code contract ([`exit`]): invalid params are usage
/// errors (`2`), and an unavailable fjall index gets an actionable hint instead of the bare
/// internal message.
pub fn run_tool(tool: &str, result: Result<CallToolResult, McpError>) -> Result<CallToolResult> {
    result.map_err(|e| {
        let message = e.message.to_string();
        if e.code == rmcp::model::ErrorCode::INVALID_PARAMS {
            return exit::CliExit::usage(format!("{tool}: {message}"));
        }
        if e.data
            .as_ref()
            .and_then(|data| data.get(crate::mcp::BUSY_DATA))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            return exit::CliExit::busy(format!("{tool}: {message}"));
        }
        if message.contains("index not available") {
            return exit::CliExit::busy(format!(
                "{tool}: {message}. Another basemind process holds the index lock and no daemon is \
                 reachable to forward to. Start the daemon with `basemind comms start` (it becomes the \
                 sole writer and serves this call), or stop the process holding the lock and retry."
            ));
        }
        anyhow::anyhow!("{tool}: {e}")
    })
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
        let forward = match &cmd {
            ToolCmd::Admin(admin) if admin::runs_in_process(admin) => context::Forward::Never,
            ToolCmd::Git(git) if git::reads_only_the_repository(git) => context::Forward::AnyView,
            _ => context::Forward::WorkingView,
        };
        let tools = context::build_tools(root, view, documents, forward).await?;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let opts = Emit {
            json,
            startup_us: u64::try_from(process_started.elapsed().as_micros()).unwrap_or(u64::MAX),
            called: Instant::now(),
        };
        match cmd {
            ToolCmd::Code(c) => code::run(&tools, c, &opts, &mut out).await?,
            ToolCmd::Git(g) => git::run(&tools, g, &opts, &mut out).await?,
            ToolCmd::Memory(m) => memory::run(&tools, m, &opts, &mut out).await?,
            ToolCmd::Admin(a) => admin::run(&tools, a, &opts, &mut out).await?,
            ToolCmd::Status => admin::run_status(&tools, &opts, &mut out).await?,
            ToolCmd::Overview(args) => overview::run(&tools, args, &opts, &mut out).await?,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A rescan refused because another process holds the workspace lock exits `3`, as every other
    /// lock contention does, not `1`.
    #[test]
    fn a_rescan_refused_for_the_workspace_lock_exits_busy() {
        let locked = crate::scanner::ScanError::Store(crate::store::StoreError::Locked {
            path: std::path::PathBuf::from(".lock"),
            holder: None,
        });
        let error = run_tool(
            "rescan",
            Err(rmcp::ErrorData::from(crate::engine::scan::rescan_error(&locked))),
        )
        .expect_err("refused");
        assert_eq!(exit::exit_code_for(&error), exit::BUSY);

        let failed = crate::scanner::ScanError::Store(crate::store::StoreError::BlobTooLarge);
        let error = run_tool(
            "rescan",
            Err(rmcp::ErrorData::from(crate::engine::scan::rescan_error(&failed))),
        )
        .expect_err("failed");
        assert_eq!(
            exit::exit_code_for(&error),
            exit::ERROR,
            "any other failure stays a runtime error"
        );
    }
}
