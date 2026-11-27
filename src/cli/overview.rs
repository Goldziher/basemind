//! `basemind overview` (#131): the repository's shape in one call. A CLI-only command answered by
//! the daemon by name when one is up (`src/mcp/cli_only.rs`, ADR-0047 decision 1); no verb tool.

use std::io::Write;

use anyhow::Result;
use clap::Args;

use super::context::CliContext;
use super::render::{Emit, emit};
use super::run_tool;
use crate::engine::Workspace;
use crate::engine::types::overview::OverviewParams;

#[derive(Args, Debug)]
pub struct OverviewArgs {
    /// Comma-separated sections: languages, layout, manifests, entry_points, largest,
    /// most_imported, routes, naming, recent. All when omitted.
    #[arg(long)]
    pub sections: Option<String>,
    /// Rows per list section (default 10, at most 100).
    #[arg(long)]
    pub limit: Option<u32>,
}

/// Run `overview` and print its answer.
pub(super) async fn run(tools: &CliContext, args: OverviewArgs, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = OverviewParams {
        sections: args.sections,
        limit: args.limit,
    };
    let result = tools
        .call("overview", p, async |s, p| s.cli_op(p, Workspace::overview).await)
        .await;
    let r = run_tool("overview", result)?;
    emit("overview", &r, opts, out)
}
