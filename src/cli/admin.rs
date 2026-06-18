//! Admin subcommands: telemetry summary + cache management.
//!
//! `telemetry` dispatches to the MCP `telemetry_summary` tool for parity.
//! The `cache` subcommands are the OFFLINE path: they call `store_gc` directly
//! (no server / flock needed) so they can safely clear `views` / `all` that the
//! in-process MCP `cache_clear` tool refuses to touch.

use std::io::Write;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::config;
use crate::mcp::BasemindServer;
use crate::mcp::params::*;
use crate::store_gc::{self, CacheComponent};

use super::render::{emit, render_human, render_json};
use super::run_tool;

#[derive(Subcommand, Debug)]
pub enum CacheCmd {
    /// Garbage-collect orphaned extraction blobs from `.basemind/blobs/`.
    Gc,
    /// Report on-disk size + blob accounting for the `.basemind/` cache.
    Stats,
    /// Clear a cache component (`blobs|views|lance|git-cache|telemetry|all`).
    ///
    /// Run with no `--component` to clear `git-cache` (back-compat with the old
    /// `basemind cache clear`).
    Clear {
        /// Component to clear. Defaults to `git-cache` for back-compat.
        #[arg(long, default_value = "git-cache")]
        component: String,
    },
}

/// Dispatch the `telemetry` subcommand (MCP tool parity).
pub async fn run_telemetry(
    server: &BasemindServer,
    window: Option<String>,
    tool: Option<String>,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    let p = TelemetrySummaryParams { window, tool };
    let r = run_tool(
        "telemetry_summary",
        server.telemetry_summary(Parameters(p)).await,
    )?;
    emit("telemetry_summary", &r, json, out)
}

/// Dispatch a `cache` subcommand against the on-disk `.basemind/` directory.
///
/// These never touch the server: they operate directly on the offline
/// `store_gc` primitives, which is why this is the only safe place to clear the
/// live Fjall index (`views` / `all`).
pub fn run_cache(root: &Path, cmd: CacheCmd, json: bool, out: &mut impl Write) -> Result<()> {
    let basemind_dir = root.join(config::BASEMIND_DIR);
    match cmd {
        CacheCmd::Gc => {
            let report = store_gc::run_gc(&basemind_dir).context("run blob GC")?;
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
        CacheCmd::Clear { component } => {
            let comp = CacheComponent::from_str(&component).map_err(|e| anyhow::anyhow!(e))?;
            store_gc::clear_component(&basemind_dir, comp)
                .with_context(|| format!("clear cache component {component}"))?;
            let value = serde_json::json!({
                "component": comp.as_str(),
                "cleared": true,
            });
            if json {
                render_json(&value, out)
            } else {
                render_human("cache_clear", &value, out)
            }
        }
    }
}
