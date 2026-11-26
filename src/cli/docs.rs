//! `basemind docs` — the CLI half of `docs_search`.
//!
//! `search` runs the body of `docs_search`, which is what `tests/cli_parity/` asserts. The same
//! arguments are accepted under the hidden `basemind memory documents`, the spelling before `docs`
//! existed.
//!
//! The tool is registered in every build and gates in the body, so a build without the `documents`
//! feature surfaces the tool's own "rebuild with --features …" error.

use std::io::Write;

use anyhow::Result;
use clap::{Args, Subcommand};

use super::context::Tools;
use super::render::{Emit, emit};
use super::run_tool;
use crate::engine::cursor::Cursor;
use crate::engine::types::search::DocsSearchParams;

#[derive(Subcommand, Debug)]
pub enum DocsCmd {
    /// Semantic search over indexed documents (needs `--features documents`).
    #[command(after_long_help = crate::cli::help::DOCS_SEARCH)]
    Search(DocsSearchArgs),
}

/// The arguments of `basemind docs search`: the fields of `docs_search`.
#[derive(Args, Debug)]
pub struct DocsSearchArgs {
    query: String,
    /// Exact MIME filter, e.g. `application/pdf`.
    #[arg(long)]
    mime_type: Option<String>,
    /// Format family: pdf, office, html, markdown, email, notebook, text, other.
    #[arg(long)]
    format: Option<String>,
    /// Repository-relative prefix, or a glob when it holds `*` (`docs/`, `docs/**/*.md`).
    #[arg(long)]
    path: Option<String>,
    /// Keep documents with any of these whole keywords (comma-separated).
    #[arg(long)]
    keywords: Option<String>,
    /// Keep chunks naming an entity of this category.
    #[arg(long)]
    entity: Option<String>,
    #[arg(long)]
    limit: Option<u32>,
    /// Resume token: pass the previous page's `next_cursor`.
    #[arg(long)]
    cursor: Option<String>,
    /// Token budget for the returned list; overflow is dropped and flagged `budgeted`.
    #[arg(long)]
    max_tokens: Option<u32>,
    /// Most hits one document contributes before other documents rank ahead of its rest; 0 is no
    /// cap (default 2).
    #[arg(long)]
    per_doc: Option<u32>,
    /// Add the chunk before and after each hit.
    #[arg(long)]
    context: bool,
}

/// Dispatch a `docs` subcommand through the in-process server.
pub async fn run(tools: &Tools, cmd: DocsCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    match cmd {
        DocsCmd::Search(args) => search(tools, args, opts, out).await,
    }
}

/// Run `docs_search` with `args` and print its answer.
pub(super) async fn search(tools: &Tools, args: DocsSearchArgs, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let p = DocsSearchParams {
        query: args.query,
        mime_type: args.mime_type,
        format: args.format,
        path: args.path,
        keywords: args.keywords,
        entity: args.entity,
        limit: args.limit,
        cursor: args.cursor.map(Cursor),
        max_tokens: args.max_tokens,
        per_doc: args.per_doc,
        context: args.context.then_some(true),
    };
    let result = tools
        .call("docs_search", p, async |s, p| s.docs_search_call(p).await)
        .await;
    let r = run_tool("docs_search", result)?;
    emit("docs_search", &r, opts, out)
}
