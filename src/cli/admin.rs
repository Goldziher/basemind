//! `basemind admin` — the CLI half of the `admin` domain, plus the offline `cache` group.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation; they map one-to-one onto the MCP `admin` tool's [`AdminMode`]
//! values, which is what `tests/cli_parity.rs` asserts.
//!
//! The `cache` subcommands below are a DIFFERENT, offline path: they call `store_gc` directly (no
//! server, no flock), which is the only way to clear `views` / `all` — the components the
//! in-process `admin cache-clear` refuses because they back the live index. They are kept
//! alongside, not folded in, precisely because the in-process tool cannot do that job.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::mcp::BasemindServer;
use crate::mcp::params::*;
use crate::path::{RelPath, normalize_query_path};
use crate::store_gc::{self, CacheComponent};

use super::render::{Emit, emit, render_human, render_json};
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum AdminCmd {
    /// Index health for this workspace: file counts, languages, scan age.
    Status,
    /// Repository identity: workdir, branch, HEAD sha.
    Repo,
    /// Re-index changed files, or the whole working tree when no paths are given.
    Rescan {
        /// Repo-relative paths to re-index incrementally. Omit to walk the whole tree.
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        /// Force a full working-tree re-index even when paths are supplied.
        #[arg(long)]
        full: bool,
    },
    /// On-disk size, blob accounting, and process RAM for the machine-global cache.
    CacheStats,
    /// Report blobs no live view references (non-destructive while the store is machine-global).
    Gc,
    /// Delete a cache component. `views`/`all` are refused here — use `basemind cache clear`.
    CacheClear {
        /// Component: `blobs|views|lance|git-cache|telemetry|all`, or `views:<name>`.
        #[arg(long, default_value = "git-cache")]
        component: String,
        /// Required gate for the destructive components that back the live code map.
        #[arg(long)]
        confirm: bool,
    },
    /// Aggregate recorded tool calls into a usage and token-savings summary.
    Telemetry {
        /// Aggregation window: `today` (default), `1h`, `24h`, `all`.
        #[arg(long)]
        window: Option<String>,
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
        /// Reduction intensity: `off|light|moderate|aggressive|maximum`.
        #[arg(long)]
        level: Option<String>,
        /// Soft token budget hint, echoed back in the response.
        #[arg(long)]
        target_tokens: Option<u32>,
        /// Let the prose pass rewrite code blocks (they are preserved by default).
        #[arg(long)]
        no_preserve_code: bool,
    },
    /// Emit a compact +N/-M line diff from a previously seen version to the current one.
    Delta {
        /// File holding the OLD (previously seen) content.
        #[arg(long, value_name = "FILE")]
        old: PathBuf,
        /// File holding the NEW content. Read from stdin when omitted.
        #[arg(long, value_name = "FILE")]
        new: Option<PathBuf>,
    },
    /// Extract decisions, errors, and changed files from session text into a checkpoint.
    Checkpoint {
        /// Session text. Read from stdin when omitted.
        #[arg(long)]
        text: Option<String>,
    },
    /// Flag repeated or redundant tool calls in a JSON-Lines tool-call log.
    Waste {
        /// File holding the JSON-Lines log. Read from stdin when omitted.
        #[arg(long, value_name = "FILE")]
        log: Option<PathBuf>,
    },
}

/// Read the whole of stdin as lossy UTF-8, so non-UTF-8 input never aborts the pipe.
fn read_stdin() -> Result<String> {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).context("read stdin")?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Read `path` as lossy UTF-8, or stdin when no path was supplied.
fn read_file_or_stdin(path: Option<&Path>) -> Result<String> {
    match path {
        Some(path) => {
            let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            Ok(String::from_utf8_lossy(&raw).into_owned())
        }
        None => read_stdin(),
    }
}

/// Resolve a user-supplied CLI path into the repo-relative key the index is keyed by, falling back
/// to the raw input so the tool reports "file not indexed" rather than silently mangling it.
fn resolve_path(server: &BasemindServer, path: &str) -> RelPath {
    match normalize_query_path(path, &server.state.shared.root) {
        Some(rel) => RelPath::from(rel),
        None => RelPath::from(path),
    }
}

/// Dispatch an `admin` subcommand through the in-process server.
pub async fn run(server: &BasemindServer, cmd: AdminCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = match cmd {
        AdminCmd::Status => AdminParams::new(AdminMode::Status),
        AdminCmd::Repo => AdminParams::new(AdminMode::Repo),
        AdminCmd::Rescan { paths, full } => AdminParams {
            paths: (!paths.is_empty()).then_some(paths),
            full: full.then_some(true),
            ..AdminParams::new(AdminMode::Rescan)
        },
        AdminCmd::CacheStats => AdminParams::new(AdminMode::CacheStats),
        AdminCmd::Gc => AdminParams::new(AdminMode::Gc),
        AdminCmd::CacheClear { component, confirm } => AdminParams {
            component: Some(component),
            confirm: confirm.then_some(true),
            ..AdminParams::new(AdminMode::CacheClear)
        },
        AdminCmd::Telemetry { window, tool } => AdminParams {
            window,
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
                level,
                target_tokens,
                preserve_code: no_preserve_code.then_some(false),
                ..AdminParams::new(AdminMode::Compress)
            }
        }
        AdminCmd::Delta { old, new } => AdminParams {
            old: Some(read_file_or_stdin(Some(&old))?),
            new: Some(read_file_or_stdin(new.as_deref())?),
            ..AdminParams::new(AdminMode::Delta)
        },
        AdminCmd::Checkpoint { text } => AdminParams {
            text: Some(match text {
                Some(text) => text,
                None => read_stdin()?,
            }),
            ..AdminParams::new(AdminMode::Checkpoint)
        },
        AdminCmd::Waste { log } => AdminParams {
            log: Some(read_file_or_stdin(log.as_deref())?),
            ..AdminParams::new(AdminMode::Waste)
        },
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.admin_cli(p).await)?;
    emit(key, &r, opts, out)
}

#[derive(Subcommand, Debug)]
pub enum CacheCmd {
    /// Garbage-collect orphaned extraction blobs from `.basemind/blobs/`.
    Gc,
    /// Report on-disk size + blob accounting for the `.basemind/` cache.
    Stats,
    /// Clear a cache component (`blobs|views|lance|git-cache|telemetry|all`), or a
    /// single view with `views:<name>` (e.g. `views:rev-abc1234`).
    ///
    /// Run with no `--component` to clear `git-cache` (back-compat with the old
    /// `basemind cache clear`).
    Clear {
        /// Component to clear (`blobs|views|lance|git-cache|telemetry|all`), or
        /// `views:<name>` for a single view. Defaults to `git-cache` for back-compat.
        #[arg(long, default_value = "git-cache")]
        component: String,
    },
}

/// Dispatch the `telemetry` subcommand (MCP tool parity).
pub async fn run_telemetry(
    server: &BasemindServer,
    window: Option<String>,
    tool: Option<String>,
    opts: &Emit,
    out: &mut impl Write,
) -> Result<()> {
    run(server, AdminCmd::Telemetry { window, tool }, opts, out).await
}

/// Dispatch a `cache` subcommand against the on-disk `.basemind/` directory.
///
/// These never touch the server: they operate directly on the offline
/// `store_gc` primitives, which is why this is the only safe place to clear the
/// live Fjall index (`views` / `all`).
pub fn run_cache(root: &Path, cmd: CacheCmd, json: bool, out: &mut impl Write) -> Result<()> {
    let basemind_dir = crate::store::workspace_cache_dir(root);
    match cmd {
        CacheCmd::Gc => {
            let report = store_gc::run_gc(&basemind_dir).context("run blob GC")?;
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
        CacheCmd::Clear { component } => {
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
        for mode in crate::mcp::mode::AdminMode::ALL_MODES {
            let expected = mode.replace('_', "-");
            assert!(
                names.contains(&expected),
                "`admin` mode `{mode}` has no `basemind admin {expected}` subcommand; got {names:?}"
            );
        }
    }
}
