//! `basemind-ui` — the desktop UI front-end for basemind (ADR-0006).
//!
//! This binary is a sibling of `basemind`, launched by the `basemind ui` subcommand exactly as
//! `basemind-tui` is launched by `basemind agent`: it ships alongside `basemind` in the release
//! archive and is re-exec'd from there.
//!
//! The interactive desktop window — a Tauri app driving the agent client/event/command seam
//! (`basemind_agent::transport::AgentClient`) and rendering the canonical graph-view payload
//! (ADR-0005) — is introduced behind a `desktop` cargo feature in a later slice, so the default
//! build (and `cargo build --workspace` / CI) stays free of the per-platform webview dependency.
//!
//! Until then this binary is the launch path's working baseline: it opens the same offline,
//! self-contained interactive HTML graph the `graph` tool's `display` mode produces (ADR-0005
//! render, ADR-0007 launch), for the selected repository — the surface the resident window will
//! replace.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use basemind::cli::context::build_server;
use basemind::cli::graph::{self, GraphCmd};
use basemind::cli::render::Emit;
use basemind::store::VIEW_WORKING;

/// Parsed `basemind-ui` arguments: just the repository root today. The graph-shaping knobs the
/// `display` mode exposes are defaulted here; the interactive window will surface them as live
/// controls in a later slice.
struct Args {
    root: PathBuf,
}

/// Parse `[--root <path>]`, defaulting the root to the current directory. Unknown flags are ignored
/// so the `basemind ui` launcher can forward future options without breaking this baseline.
fn parse_args<I: Iterator<Item = String>>(args: I) -> Args {
    let mut root = PathBuf::from(".");
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg == "--root"
            && let Some(value) = args.next()
        {
            root = PathBuf::from(value);
        }
    }
    Args { root }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1));
    open_graph_window(&args.root).await
}

/// Render the repository's code graph as the offline interactive HTML view and open it in the
/// human's default viewer, reusing the shared `graph display` path (ADR-0005 payload, ADR-0007
/// launch). This is the baseline the resident Tauri window replaces.
async fn open_graph_window(root: &Path) -> Result<()> {
    let server = build_server(root, VIEW_WORKING, Default::default())
        .context("open the basemind code map (run `basemind scan` first)")?;
    let command = GraphCmd::Display {
        format: "html".to_string(),
        focus: None,
        edges: "all".to_string(),
        algorithm: "label_propagation".to_string(),
        min_confidence: None,
        max_nodes: None,
        no_open: false,
    };
    let emit = Emit {
        json: false,
        startup_us: 0,
    };
    let mut out = std::io::stdout().lock();
    graph::run(&server, command, &emit, &mut out)
        .await
        .context("render and open the code graph")
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn parse_args_defaults_root_to_cwd() {
        let args = parse_args(std::iter::empty());
        assert_eq!(args.root, std::path::PathBuf::from("."));
    }

    #[test]
    fn parse_args_reads_an_explicit_root() {
        let args = parse_args(["--root", "/tmp/repo"].into_iter().map(String::from));
        assert_eq!(args.root, std::path::PathBuf::from("/tmp/repo"));
    }

    #[test]
    fn parse_args_ignores_unknown_flags_and_keeps_root() {
        let args = parse_args(["--future", "x", "--root", "/r"].into_iter().map(String::from));
        assert_eq!(args.root, std::path::PathBuf::from("/r"));
    }
}
