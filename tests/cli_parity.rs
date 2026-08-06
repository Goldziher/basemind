//! Executable CLI↔MCP parity guard.
//!
//! basemind's contract is that every MCP `#[tool]` an agent can call over stdio is ALSO reachable
//! from the `basemind` CLI (agents and tests drive both). This test makes that contract enforceable:
//!
//! 1. It enumerates the live MCP tool surface from the in-process server
//!    ([`BasemindServer::tool_names`]) — the exact set `tools/list` advertises.
//! 2. It cross-references that set against `TOOL_TO_CLI`, a maintained table mapping each tool —
//!    and, for a consolidated domain tool, each `mode` of that tool — to the CLI command that
//!    invokes it.
//! 3. It asserts the mapping is a bijection (every tool mapped, every mapping real), that every
//!    advertised `(domain, mode)` pair is mapped, and that each mapped CLI path actually resolves
//!    (`basemind <path> --help` exits 0).
//!
//! A new tool shipped without its CLI counterpart fails step 2 (unmapped tool); a renamed/removed
//! CLI command fails step 3. The table is feature-gated the same way the routers are, so the guard
//! is exact under whatever feature set the test is compiled with.
//!
//! **Why the table is keyed on `(tool, mode)` and not on the tool name.** basemind's operations live
//! in a required `mode` enum behind nine domain tools, so a name-keyed table would verify nine names
//! and silently stop covering the dozens of operations beneath them — exactly the coverage this
//! guard exists to provide. `mode_coverage_is_exact` closes that hole by walking
//! [`basemind::mcp::mode::domain_modes`], which is generated from the same enums the schemas are.

use std::process::Command;

use basemind::cli::context::build_server;
use basemind::config::DocumentsCliOverrides;
use basemind::store::VIEW_WORKING;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

/// The intended MCP-operation → CLI-command mapping: `(tool, mode, cli)`. `mode` is `Some` for a
/// consolidated domain tool and `None` for a tool that takes no `mode`. The CLI value is the
/// argument path (sans the `basemind` prefix and any positional operands) used to reach the
/// identical tool code. Grouped by router; feature-gated groups mirror the `#[cfg]` on their
/// `tool_router_*` registration in `src/mcp/mod.rs` so the set matches `tool_names()` exactly under
/// any feature build.
fn tool_to_cli() -> Vec<(&'static str, Option<&'static str>, &'static str)> {
    #[allow(unused_mut)]
    let mut m: Vec<(&str, Option<&str>, &str)> = vec![
        ("code", Some("outline"), "code outline"),
        ("code", Some("symbols"), "code symbols"),
        ("code", Some("grep"), "code grep"),
        ("code", Some("files"), "code files"),
        ("code", Some("find"), "code find"),
        ("code", Some("definition"), "code definition"),
        ("code", Some("references"), "code references"),
        ("code", Some("callers"), "code callers"),
        ("code", Some("implementations"), "code implementations"),
        ("code", Some("dependents"), "code dependents"),
        ("code", Some("expand"), "code expand"),
        ("code", Some("semantic"), "code semantic"),
        ("code", Some("chunk"), "code chunk"),
        ("graph", Some("calls"), "graph calls"),
        ("graph", Some("neighbors"), "graph neighbors"),
        ("graph", Some("path"), "graph path"),
        ("graph", Some("subgraph"), "graph subgraph"),
        ("graph", Some("communities"), "graph communities"),
        ("graph", Some("map"), "graph map"),
        ("graph", Some("export"), "graph export"),
        ("graph", Some("display"), "graph display"),
        ("graph", Some("open"), "graph open"),
        ("admin", Some("status"), "admin status"),
        ("admin", Some("repo"), "admin repo"),
        ("admin", Some("rescan"), "admin rescan"),
        ("admin", Some("cache_stats"), "admin cache-stats"),
        ("admin", Some("gc"), "admin gc"),
        ("admin", Some("cache_clear"), "admin cache-clear"),
        ("admin", Some("telemetry"), "admin telemetry"),
        ("admin", Some("compress"), "admin compress"),
        ("admin", Some("delta"), "admin delta"),
        ("admin", Some("checkpoint"), "admin checkpoint"),
        ("admin", Some("waste"), "admin waste"),
        ("git", Some("status"), "git status"),
        ("git", Some("recent"), "git recent"),
        ("git", Some("touching"), "git touching"),
        ("git", Some("by_path"), "git by-path"),
        ("git", Some("churn"), "git churn"),
        ("git", Some("diff"), "git diff"),
        ("git", Some("diff_outline"), "git diff-outline"),
        ("git", Some("blame"), "git blame"),
        ("git", Some("blame_symbol"), "git blame-symbol"),
        ("git", Some("symbol_history"), "git symbol-history"),
        ("git", Some("search"), "git search"),
        ("memory", Some("put"), "memory put"),
        ("memory", Some("get"), "memory get"),
        ("memory", Some("list"), "memory list"),
        ("memory", Some("search"), "memory search"),
        ("memory", Some("delete"), "memory delete"),
        ("memory", Some("audit"), "memory audit"),
        ("memory", Some("documents"), "memory documents"),
        ("memory", Some("mine"), "memory mine"),
        ("memory", Some("proposals"), "memory proposals"),
        ("memory", Some("accept"), "memory accept"),
        ("memory", Some("reject"), "memory reject"),
    ];
    #[cfg(feature = "crawl")]
    m.extend([
        ("web", Some("scrape"), "web scrape"),
        ("web", Some("crawl"), "web crawl"),
        ("web", Some("map"), "web map"),
    ]);
    #[cfg(all(feature = "comms", any(unix, windows)))]
    m.extend([
        ("agents", Some("register"), "agents register"),
        ("agents", Some("list"), "agents list"),
        ("agents", Some("thread_start"), "agents thread-start"),
        ("agents", Some("thread_list"), "agents thread-list"),
        ("agents", Some("join"), "agents join"),
        ("agents", Some("leave"), "agents leave"),
        ("agents", Some("members"), "agents members"),
        ("agents", Some("add_member"), "agents add-member"),
        ("agents", Some("remove_member"), "agents remove-member"),
        ("agents", Some("archive"), "agents archive"),
        ("agents", Some("post"), "agents post"),
        ("agents", Some("history"), "agents history"),
        ("agents", Some("message"), "agents message"),
        ("agents", Some("inbox"), "agents inbox"),
        ("agents", Some("ack"), "agents ack"),
        ("agents", Some("wait"), "agents wait"),
        ("workspace", Some("workspaces"), "workspace workspaces"),
        ("workspace", Some("worktrees"), "workspace worktrees"),
        ("workspace", Some("branches"), "workspace branches"),
        ("workspace", Some("claim"), "workspace claim"),
        ("workspace", Some("release"), "workspace release"),
    ]);
    #[cfg(all(feature = "shells", any(unix, windows)))]
    m.extend([
        ("shell", Some("spawn"), "shell spawn"),
        ("shell", Some("send"), "shell send"),
        ("shell", Some("capture"), "shell capture"),
        ("shell", Some("kill"), "shell kill"),
        ("shell", Some("list"), "shell list"),
        ("shell", Some("broadcast"), "shell broadcast"),
    ]);
    m
}

/// Build a one-shot server over an empty tempdir just to read its advertised tool set. The working
/// view opens read-only even when never scanned, so no fixture repo is needed.
fn advertised_tools() -> Vec<String> {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let server =
        build_server(tmp.path(), VIEW_WORKING, DocumentsCliOverrides::default()).expect("build one-shot server");
    server.tool_names()
}

#[test]
fn every_mcp_tool_has_a_cli_command() {
    let tools = advertised_tools();
    let map = tool_to_cli();
    let mapped: std::collections::HashSet<&str> = map.iter().map(|(tool, _, _)| *tool).collect();

    let unmapped: Vec<&String> = tools.iter().filter(|t| !mapped.contains(t.as_str())).collect();
    assert!(
        unmapped.is_empty(),
        "MCP tools with no CLI mapping (add the CLI command + a TOOL_TO_CLI row): {unmapped:?}"
    );

    let live: std::collections::HashSet<&str> = tools.iter().map(String::as_str).collect();
    let stale: Vec<&str> = map
        .iter()
        .map(|(tool, _, _)| *tool)
        .filter(|t| !live.contains(t))
        .collect();
    assert!(
        stale.is_empty(),
        "TOOL_TO_CLI rows for tools no longer advertised (remove or rename them): {stale:?}"
    );
}

/// Parity at operation granularity. Tool-name coverage alone would pass with a single row per
/// domain, leaving every mode but one unreachable from the CLI and unnoticed — the modes are where
/// the operations went, so this is where the bijection has to be checked.
#[test]
fn every_advertised_mode_has_a_cli_command() {
    let map = tool_to_cli();

    for (domain, modes) in basemind::mcp::mode::domain_modes() {
        let rows: Vec<&str> = map
            .iter()
            .filter(|(tool, _, _)| *tool == domain)
            .filter_map(|(_, mode, _)| *mode)
            .collect();

        let unmapped: Vec<&&str> = modes.iter().filter(|m| !rows.contains(m)).collect();
        assert!(
            unmapped.is_empty(),
            "`{domain}` modes with no CLI mapping (add the subcommand + a TOOL_TO_CLI row): {unmapped:?}"
        );

        let stale: Vec<&&str> = rows.iter().filter(|m| !modes.contains(m)).collect();
        assert!(
            stale.is_empty(),
            "TOOL_TO_CLI rows for `{domain}` modes the tool no longer accepts: {stale:?}"
        );
    }
}

#[test]
fn every_mapped_cli_command_resolves() {
    for (tool, mode, cli) in tool_to_cli() {
        let mut args: Vec<&str> = cli.split(' ').collect();
        args.push("--help");
        let output = Command::new(bin())
            .args(&args)
            .output()
            .unwrap_or_else(|e| panic!("spawn `basemind {cli} --help`: {e}"));
        let operation = match mode {
            Some(mode) => format!("{tool}:{mode}"),
            None => tool.to_string(),
        };
        assert!(
            output.status.success(),
            "`basemind {cli} --help` (for `{operation}`) exited {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
