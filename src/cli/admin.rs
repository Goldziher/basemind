//! `basemind admin` — administrative commands with no MCP tool (`telemetry`, `tokens`, `eval`),
//! plus the offline `cache` group. The index tools live under their own top-level commands:
//! `status` (`index_status`) and `rescan` (`index_rescan`).
//!
//! The `cache` subcommands call `store_gc` directly (no server, no flock), which is the only way to
//! clear `views` / `all`, the components that back the live index.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;
use crate::store_gc::{self, CacheComponent};

use super::choices::TelemetryWindow;
use super::exit::CliExit;
use super::render::{Emit, emit, render_human, render_json};
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum AdminCmd {
    /// Index health plus repository identity (branch, HEAD sha). Same as `basemind status`.
    Repo,
    /// Aggregate recorded tool calls into a usage and token-savings summary.
    Telemetry {
        /// Aggregation window (default: today).
        #[arg(long, value_enum)]
        window: Option<TelemetryWindow>,
        /// Optional exact tool-name filter.
        #[arg(long)]
        tool: Option<String>,
    },
    /// Count tokens in text with the real o200k tokenizer and print just the integer (needs the
    /// `tokenizer` feature, which `documents` also includes). No `--json` envelope and no MCP
    /// round trip — this is the CLI-only primitive `benchmarks/run.sh` scripts against so both
    /// sides of a benchmark task get scored with the same tokenizer.
    Tokens {
        /// Read text from stdin. Currently the only input source; kept as an explicit flag so
        /// `basemind admin tokens --stdin <<< "$text"` reads as intentional in scripts.
        #[arg(long)]
        stdin: bool,
    },
    /// Retrieval-quality + token-savings evaluation over a JSONL task file: scores each tool's
    /// answer against gold (P/R/F1, hit@k, MRR, nDCG), measures latency and response tokens, and
    /// compares against a grep/read baseline. See `benchmarks/eval/README.md`.
    Eval(crate::eval::EvalArgs),
}

/// Count tokens in stdin and print just the integer, so a shell script can pipe straight into it.
/// Bypasses the tool dispatch entirely: it is a CLI-only convenience over [`crate::mcp::tokens::count_tokens`], not an agent-facing operation.
#[cfg(feature = "tokenizer")]
fn run_tokens(out: &mut impl Write) -> Result<()> {
    let text = read_stdin()?;
    let count = crate::mcp::tokens::count_tokens(&text);
    writeln!(out, "{count}").context("write token count")?;
    Ok(())
}

/// `tokenizer` (and therefore `documents`) was not compiled in: fail loudly rather than silently
/// falling back to the `bytes/4` heuristic, which would quietly poison any benchmark comparing
/// basemind against a baseline.
#[cfg(not(feature = "tokenizer"))]
fn run_tokens(_out: &mut impl Write) -> Result<()> {
    anyhow::bail!(
        "`admin tokens` requires the `tokenizer` feature, which is not compiled into this \
         basemind binary. Rebuild with `--features tokenizer` (or `--features documents`, which \
         includes it)."
    )
}

/// Read the whole of stdin as lossy UTF-8, so non-UTF-8 input never aborts the pipe.
#[cfg(feature = "tokenizer")]
fn read_stdin() -> Result<String> {
    use std::io::Read;

    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).context("read stdin")?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Dispatch an `admin` subcommand through the in-process server.
pub async fn run(server: &BasemindServer, cmd: AdminCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let (tool, result) = match cmd {
        AdminCmd::Repo => ("index_status", server.index_status_call(IndexStatusParams {}).await),
        AdminCmd::Telemetry { window, tool } => (
            "telemetry",
            server
                .telemetry_call(TelemetrySummaryParams {
                    window: window.map(|w| w.as_str().to_string()),
                    tool,
                })
                .await,
        ),
        AdminCmd::Tokens { .. } => return run_tokens(out),
        AdminCmd::Eval(args) => return crate::eval::run(server, &args, out).await,
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)
}

#[derive(Subcommand, Debug)]
pub enum CacheCmd {
    /// Garbage-collect orphaned extraction blobs from the machine-global cache (blobs younger than 6 h are kept;
    /// override with `BASEMIND_BLOB_GC_GRACE_SECS`).
    Gc {
        /// Only count the blobs in the store; delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Report on-disk size + blob accounting for the machine-global cache.
    Stats,
    /// Clear a cache component (`blobs|views|lance|git-cache|telemetry|all`), or a
    /// single view with `views:<name>` (e.g. `views:rev-abc1234`).
    ///
    /// Run with no `--component` to clear `git-cache` (back-compat with the old
    /// `basemind cache clear`).
    ///
    /// Everything except `git-cache` is destructive: it asks for confirmation on a terminal and
    /// requires `--yes` otherwise. `blobs` is the MACHINE-GLOBAL extraction store shared by every
    /// workspace on this machine, not just this repo. The command refuses (exit 3) while a writer
    /// holds this workspace's lock, and for `blobs` while the comms daemon is running.
    Clear {
        /// Component to clear (`blobs|views|lance|git-cache|telemetry|all`), or
        /// `views:<name>` for a single view. Defaults to `git-cache` for back-compat.
        #[arg(long, default_value = "git-cache")]
        component: String,
        /// Skip the confirmation prompt (required when stdin is not a terminal).
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

/// `basemind status`: index health plus repository identity, the body of the `index_status` tool.
pub async fn run_status(server: &BasemindServer, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let r = run_tool("index_status", server.index_status_call(IndexStatusParams {}).await)?;
    emit("index_status", &r, opts, out)
}

/// Dispatch a `cache` subcommand against the machine-global cache directories.
///
/// These never touch the server: they operate directly on the offline
/// `store_gc` primitives, which is why this is the only safe place to clear the
/// live Fjall index (`views` / `all`).
pub fn run_cache(root: &Path, cmd: CacheCmd, json: bool, out: &mut impl Write) -> Result<()> {
    let basemind_dir = crate::store::workspace_cache_dir(root);
    match cmd {
        CacheCmd::Gc { dry_run } => {
            let report = if dry_run {
                store_gc::gc_report_only().context("count blobs")?
            } else {
                store_gc::run_gc(&basemind_dir).context("run blob GC")?
            };
            let value = serde_json::to_value(&report).context("serialize GC report")?;
            if json {
                render_json(&value, out)
            } else {
                render_human("cache_gc", &value, out)
            }
        }
        CacheCmd::Stats => {
            let stats = store_gc::cache_stats(&basemind_dir).context("collect cache stats")?;
            let value = serde_json::to_value(&stats).context("serialize cache stats")?;
            if json {
                render_json(&value, out)
            } else {
                render_human("cache_stats", &value, out)
            }
        }
        CacheCmd::Clear { component, yes } => {
            guard_clear(&basemind_dir, &component, yes)?;
            let value = if let Some(name) = component.strip_prefix("views:") {
                store_gc::clear_single_view(&basemind_dir, name)
                    .with_context(|| format!("clear single view {name}"))?;
                serde_json::json!({ "component": format!("views:{name}"), "cleared": true })
            } else {
                let comp = CacheComponent::from_str(&component).map_err(|e| anyhow::anyhow!(e))?;
                store_gc::clear_component(&basemind_dir, comp)
                    .with_context(|| format!("clear cache component {component}"))?;
                serde_json::json!({ "component": comp.as_str(), "cleared": true })
            };
            if json {
                render_json(&value, out)
            } else {
                render_human("cache_clear", &value, out)
            }
        }
    }
}

/// Refuse or confirm a destructive `cache clear` before touching disk.
///
/// `git-cache` is a cheap, regenerable cache and clears unprompted. Anything else first checks that
/// no writer is using what it would delete — this workspace's lock for every component, and the
/// running daemon for `blobs`, the one component shared by all workspaces — then asks the user (on
/// a terminal) or demands `--yes` (anywhere else). Refusals carry the exit-code contract: `3` for a
/// busy writer, `2` for a missing confirmation.
fn guard_clear(basemind_dir: &Path, component: &str, yes: bool) -> Result<()> {
    if component == CacheComponent::GitCache.as_str() {
        return Ok(());
    }
    if let crate::store::WriterProbe::Held { holder } = crate::store::probe_writer_lock(basemind_dir) {
        let who = holder.map_or_else(
            || "another basemind process".to_string(),
            |meta| format!("`{}` (pid {})", meta.command, meta.pid),
        );
        return Err(CliExit::busy(format!(
            "refusing to clear `{component}`: {who} holds this workspace's index lock. Stop it first \
             (`basemind comms stop` for the daemon), then retry."
        )));
    }
    let global = component == CacheComponent::Blobs.as_str();
    if global && super::rescan::daemon_is_up() {
        return Err(CliExit::busy(
            "refusing to clear `blobs`: the basemind daemon is running and reads the machine-global blob \
             store on behalf of every workspace. Stop it with `basemind comms stop`, then retry.",
        ));
    }
    if yes {
        return Ok(());
    }
    let what = if global {
        format!(
            "`blobs` is the MACHINE-GLOBAL extraction store ({}) shared by EVERY workspace on this machine; \
             clearing it forces all of them to re-extract on their next scan",
            crate::store::global_blobs_dir().display()
        )
    } else {
        format!("clear `{component}` under {}", basemind_dir.display())
    };
    if !std::io::stdin().is_terminal() {
        return Err(CliExit::usage(format!(
            "refusing to {what} without confirmation; pass --yes to proceed"
        )));
    }
    eprint!("About to {what}. Continue? [y/N] ");
    std::io::stderr().flush().context("flush prompt")?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).context("read confirmation")?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(CliExit::usage("aborted: nothing was cleared"))
    }
}
