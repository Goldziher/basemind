//! `basemind memory` — the CLI half of `memory_save` and `memory_search`.
//!
//! `put` runs the body of `memory_save` and `search` / `get` / `list` the body of `memory_search`,
//! which is what `tests/cli_parity/` asserts. `delete`, `clear`, `export` and `import` have no MCP tool. The hidden
//! `documents` is `basemind docs search` ([`super::docs`]) under its earlier name.
//!
//! The tools are registered in every build and gate in the body, so these handlers always compile
//! and a command whose feature is missing surfaces the tool's own "rebuild with --features …"
//! error.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use super::context::Tools;
use crate::engine::Workspace;
use crate::engine::types::memory::{ImportPolicy, MemoryExportParams, MemoryImportParams, TRANSFER_MAX_BYTES};
use crate::mcp::cursor::Cursor;
use crate::mcp::params::*;

use super::render::{Emit, emit};
use super::run_tool;

/// The memory tier a `--individual` flag selects (`None` → the default group tier).
fn visibility(individual: bool) -> Option<Visibility> {
    individual.then_some(Visibility::Individual)
}

#[derive(Subcommand, Debug)]
pub enum MemoryCmd {
    /// Store a key/value memory entry.
    #[command(after_long_help = crate::cli::help::MEMORY_PUT)]
    Put {
        key: String,
        value: String,
        /// Tag for filtered recall (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Store in the per-agent individual tier instead of the shared group tier.
        #[arg(long)]
        individual: bool,
    },
    /// Semantic search over memory.
    #[command(after_long_help = crate::cli::help::MEMORY_SEARCH)]
    Search {
        query: String,
        #[arg(long)]
        limit: Option<u32>,
        /// Keep notes carrying any of these tags (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        individual: bool,
    },
    /// Read a single memory entry by key. Shorthand for one shape of `memory_search`.
    #[command(after_long_help = crate::cli::help::MEMORY_GET)]
    Get {
        key: String,
        #[arg(long)]
        individual: bool,
    },
    /// List memory entries by key prefix and/or tags. Shorthand for one shape of `memory_search`.
    #[command(after_long_help = crate::cli::help::MEMORY_LIST)]
    List {
        #[arg(long)]
        prefix: Option<String>,
        /// Keep notes carrying any of these tags (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        individual: bool,
        /// Resume token: pass the previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Delete a memory entry by key (CLI only).
    #[command(after_long_help = crate::cli::help::MEMORY_DELETE)]
    Delete {
        key: String,
        #[arg(long)]
        individual: bool,
    },
    /// Remove every memory note of this repository, of every tier and agent, and their vectors (CLI
    /// only). Asks for confirmation on a terminal and requires `--yes` otherwise.
    #[command(after_long_help = crate::cli::help::MEMORY_CLEAR)]
    Clear {
        /// Skip the confirmation prompt (required when stdin is not a terminal).
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Write this repository's notes of one tier to a file (CLI only).
    #[command(after_long_help = crate::cli::help::MEMORY_EXPORT)]
    Export {
        /// Write to this file instead of stdout.
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        /// Only notes whose key starts with this.
        #[arg(long)]
        prefix: Option<String>,
        /// Only notes carrying any of these tags (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        individual: bool,
    },
    /// Merge the notes of an export file into this repository (CLI only).
    #[command(after_long_help = crate::cli::help::MEMORY_IMPORT)]
    Import {
        /// The export file; `-` reads stdin.
        file: PathBuf,
        /// Replace a note the repository already has with the file's, newer or not.
        #[arg(long, conflicts_with = "skip")]
        overwrite: bool,
        /// Keep every note the repository already has.
        #[arg(long)]
        skip: bool,
        #[arg(long)]
        individual: bool,
    },
    /// `basemind docs search`, under the name it had before `docs` existed.
    #[command(hide = true)]
    Documents(super::docs::DocsSearchArgs),
}

/// A `memory_search` call with no selector set.
fn recall(individual: bool) -> MemorySearchToolParams {
    MemorySearchToolParams {
        query: None,
        key: None,
        prefix: None,
        tags: None,
        visibility: visibility(individual),
        limit: None,
        cursor: None,
    }
}

/// What `basemind memory clear` asks to confirm for the repository at `root`.
fn clear_prompt(root: &Path) -> String {
    format!(
        "remove every memory note of this repository, of every tier and agent, and their vectors ({})",
        crate::store_layout::repo_group_dir(root).display()
    )
}

/// Dispatch a `memory` subcommand through the in-process server.
pub async fn run(tools: &Tools, cmd: MemoryCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    let (tool, result) = match cmd {
        MemoryCmd::Put {
            key,
            value,
            tags,
            individual,
        } => {
            let p = MemorySaveParams {
                key,
                value,
                tags: (!tags.is_empty()).then_some(tags),
                visibility: visibility(individual),
            };
            (
                "memory_save",
                tools
                    .call("memory_save", p, async |s, p| s.memory_save_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Search {
            query,
            limit,
            tags,
            individual,
        } => {
            let p = MemorySearchToolParams {
                query: Some(query),
                limit,
                tags: (!tags.is_empty()).then_some(tags),
                ..recall(individual)
            };
            (
                "memory_search",
                tools
                    .call("memory_search", p, async |s, p| s.memory_search_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Get { key, individual } => {
            let p = MemorySearchToolParams {
                key: Some(key),
                ..recall(individual)
            };
            (
                "memory_search",
                tools
                    .call("memory_search", p, async |s, p| s.memory_search_call(p).await)
                    .await,
            )
        }
        MemoryCmd::List {
            prefix,
            tags,
            limit,
            individual,
            cursor,
        } => {
            let p = MemorySearchToolParams {
                prefix,
                tags: (!tags.is_empty()).then_some(tags),
                limit,
                cursor: cursor.map(Cursor),
                ..recall(individual)
            };
            (
                "memory_search",
                tools
                    .call("memory_search", p, async |s, p| s.memory_search_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Delete { key, individual } => {
            let p = MemoryDeleteParams {
                key,
                visibility: visibility(individual).unwrap_or_default(),
            };
            (
                "memory_delete",
                tools
                    .call("memory_delete", p, async |s, p| s.memory_delete_call(p).await)
                    .await,
            )
        }
        MemoryCmd::Clear { yes } => {
            if !yes {
                super::admin::confirm(&clear_prompt(tools.root()), std::io::stdin().is_terminal())?;
            }
            (
                "memory_clear",
                tools
                    .call("memory_clear", MemoryClearParams {}, async |s, p| {
                        s.memory_clear_call(p).await
                    })
                    .await,
            )
        }
        MemoryCmd::Export {
            output,
            prefix,
            tags,
            individual,
        } => return export(tools, output, prefix, tags, individual, opts, out).await,
        MemoryCmd::Import {
            file,
            overwrite,
            skip,
            individual,
        } => {
            let policy = match (overwrite, skip) {
                (true, _) => ImportPolicy::Overwrite,
                (_, true) => ImportPolicy::Skip,
                _ => ImportPolicy::Newer,
            };
            let (content, source) = read_import(&file)?;
            let p = MemoryImportParams {
                content,
                policy,
                source: Some(source),
                visibility: visibility(individual).unwrap_or_default(),
            };
            (
                "memory_import",
                tools
                    .call("memory_import", p, async |s, p| {
                        s.cli_op(p, Workspace::memory_import).await
                    })
                    .await,
            )
        }
        MemoryCmd::Documents(args) => return super::docs::search(tools, args, opts, out).await,
    };

    let r = run_tool(tool, result)?;
    emit(tool, &r, opts, out)
}

/// `basemind memory export`: the file's text to `output`, or to stdout.
async fn export(
    tools: &Tools,
    output: Option<PathBuf>,
    prefix: Option<String>,
    tags: Vec<String>,
    individual: bool,
    opts: &Emit,
    out: &mut impl Write,
) -> Result<()> {
    let p = MemoryExportParams {
        prefix,
        tags,
        visibility: visibility(individual).unwrap_or_default(),
    };
    let result = tools
        .call("memory_export", p, async |s, p| {
            s.cli_op(p, Workspace::memory_export).await
        })
        .await;
    let reply = super::render::result_to_value(&run_tool("memory_export", result)?)?;
    let content = reply["content"]
        .as_str()
        .context("memory export: the reply holds no content")?;
    match output {
        Some(path) => {
            std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
            let summary = serde_json::json!({ "notes": reply["notes"], "path": path.display().to_string() });
            if opts.json {
                super::render::render_json(&summary, out)
            } else {
                writeln!(out, "exported {} notes to {}", reply["notes"], path.display()).map_err(Into::into)
            }
        }
        None => out.write_all(content.as_bytes()).map_err(Into::into),
    }
}

/// The text of an import file (`-` is stdin) and the label recorded as its source. A file past
/// [`TRANSFER_MAX_BYTES`] is refused before it is read whole.
fn read_import(file: &Path) -> Result<(String, String)> {
    use std::io::Read;

    let (mut reader, source): (Box<dyn Read>, String) = if file == Path::new("-") {
        (Box::new(std::io::stdin()), "stdin".to_string())
    } else {
        let handle = std::fs::File::open(file).with_context(|| format!("open {}", file.display()))?;
        let name = file
            .file_name()
            .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());
        (Box::new(handle), name)
    };
    let mut content = String::new();
    reader
        .by_ref()
        .take(TRANSFER_MAX_BYTES as u64 + 1)
        .read_to_string(&mut content)
        .with_context(|| format!("read {}", file.display()))?;
    anyhow::ensure!(
        content.len() <= TRANSFER_MAX_BYTES,
        "memory import: {} is larger than {TRANSFER_MAX_BYTES} bytes; split it with `memory export --prefix` or `--tag`",
        file.display()
    );
    Ok((content, source))
}
