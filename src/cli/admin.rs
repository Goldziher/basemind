//! `basemind admin` — the CLI half of the `admin` domain that has no better top-level home, plus the
//! offline `cache` group.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation. The other `admin` modes live under their canonical command:
//! `status`, `rescan`, `cache stats|gc|clear`, `delta`, `checkpoint` and `detect-waste`.
//!
//! The `cache` subcommands call `store_gc` directly (no server, no flock), which is the only way to
//! clear `views` / `all`, the components that back the live index.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;
use crate::store_gc::{self, CacheComponent};

use super::choices::{CompressLevel, TelemetryWindow};
use super::exit::CliExit;
use super::render::{Emit, emit, render_human, render_json};
use super::{resolve_path, run_tool};

#[derive(Subcommand, Debug)]
pub enum AdminCmd {
    /// Repository identity: workdir, branch, HEAD sha.
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
    /// Shrink content for re-use in a smaller context: a file's outline, or a prose pass.
    Compress {
        /// Indexed source file to compress structurally. Mutually exclusive with `--text`.
        #[arg(long)]
        path: Option<String>,
        /// Prose to compress. Read from stdin when neither this nor `--path` is given.
        #[arg(long)]
        text: Option<String>,
        /// Reduction intensity.
        #[arg(long, value_enum)]
        level: Option<CompressLevel>,
        /// Soft token budget hint, echoed back in the response.
        #[arg(long)]
        target_tokens: Option<u32>,
        /// Let the prose pass rewrite code blocks (they are preserved by default).
        #[arg(long)]
        no_preserve_code: bool,
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
/// Bypasses the MCP tool dispatch entirely — there is no `AdminMode` for this, since it is a
/// CLI-only convenience over [`crate::mcp::tokens::count_tokens`], not an agent-facing operation.
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
fn read_stdin() -> Result<String> {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).context("read stdin")?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Dispatch an `admin` subcommand through the in-process server.
pub async fn run(server: &BasemindServer, cmd: AdminCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = match cmd {
        AdminCmd::Repo => AdminParams::new(AdminMode::Repo),
        AdminCmd::Telemetry { window, tool } => AdminParams {
            window: window.map(|w| w.as_str().to_string()),
            tool,
            ..AdminParams::new(AdminMode::Telemetry)
        },
        AdminCmd::Compress {
            path,
            text,
            level,
            target_tokens,
            no_preserve_code,
        } => {
            let resolved = path.as_deref().map(|p| resolve_path(server, p));
            let text = match (&resolved, text) {
                (Some(_), text) => text,
                (None, Some(text)) => Some(text),
                (None, None) => Some(read_stdin()?),
            };
            AdminParams {
                path: resolved,
                text,
                level: level.map(|l| l.as_str().to_string()),
                target_tokens,
                preserve_code: no_preserve_code.then_some(false),
                ..AdminParams::new(AdminMode::Compress)
            }
        }
        AdminCmd::Tokens { .. } => return run_tokens(out),
        AdminCmd::Eval(args) => return crate::eval::run(server, &args, out).await,
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.admin_cli(p).await)?;
    emit(key, &r, opts, out)
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

/// Dispatch the top-level `status` command: index health for this workspace.
pub async fn run_status(server: &BasemindServer, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let key = AdminMode::Status.telemetry_key();
    let r = run_tool(key, server.admin_cli(AdminParams::new(AdminMode::Status)).await)?;
    emit(key, &r, opts, out)
}

/// Dispatch a `cache` subcommand against the on-disk `.basemind/` directory.
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
                render_human("admin:gc", &value, out)
            }
        }
        CacheCmd::Stats => {
            let stats = store_gc::cache_stats(&basemind_dir).context("collect cache stats")?;
            let value = serde_json::to_value(&stats).context("serialize cache stats")?;
            if json {
                render_json(&value, out)
            } else {
                render_human("admin:cache_stats", &value, out)
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
                render_human("admin:cache_clear", &value, out)
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

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, Subcommand as _};

    use super::AdminCmd;

    #[derive(Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: AdminCmd,
    }

    /// The CLI half of the parity contract, checked from this side too: every `admin` mode the MCP
    /// tool advertises must resolve to a clap subcommand of the same (kebab-cased) name.
    /// `tests/cli_parity.rs` proves the same thing end-to-end, but only for a build that ships the
    /// binary — this one fails fast, in the file that owns the enum.
    #[test]
    fn should_expose_one_subcommand_per_advertised_admin_mode() {
        let command = AdminCmd::augment_subcommands(Harness::command());
        let names: Vec<String> = command.get_subcommands().map(|s| s.get_name().to_string()).collect();
        // Modes whose CLI home is a top-level command (`tests/cli_parity/` maps them).
        const TOP_LEVEL: &[&str] = &[
            "status",
            "rescan",
            "cache_stats",
            "gc",
            "cache_clear",
            "delta",
            "checkpoint",
            "waste",
        ];
        for mode in crate::mcp::mode::AdminMode::ALL_MODES
            .iter()
            .filter(|m| !TOP_LEVEL.contains(m))
        {
            let expected = mode.replace('_', "-");
            assert!(
                names.contains(&expected),
                "`admin` mode `{mode}` has no `basemind admin {expected}` subcommand; got {names:?}"
            );
        }
    }
}
