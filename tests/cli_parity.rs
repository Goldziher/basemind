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
        ("outline", None, "query outline"),
        ("search_symbols", None, "query search"),
        ("find_references", None, "query references"),
        ("find_callers", None, "query callers"),
        ("goto_definition", None, "query goto-definition"),
        ("find_implementations", None, "query implementations"),
        ("call_graph", None, "query call-graph"),
        ("architecture_map", None, "query architecture-map"),
        ("neighbors", None, "query neighbors"),
        ("path", None, "query path"),
        ("subgraph", None, "query subgraph"),
        ("communities", None, "query communities"),
        ("graph_export", None, "query graph-export"),
        ("display", None, "query display"),
        ("ui", None, "query ui"),
        ("workspace_grep", None, "query grep"),
        ("list_files", None, "query list-files"),
        ("find_files", None, "query find-files"),
        ("dependents", None, "query dependents"),
        ("status", None, "query status"),
        ("repo_info", None, "query repo-info"),
        ("symbol_history", None, "git symbol-history"),
        ("rescan", None, "rescan"),
        ("telemetry_summary", None, "telemetry"),
        ("search_code", None, "query search-code"),
        ("get_chunk", None, "query get-chunk"),
        ("expand", None, "query expand"),
        ("compress", None, "compress-output"),
        ("delta", None, "delta"),
        ("checkpoint", None, "checkpoint"),
        ("detect_waste", None, "detect-waste"),
        ("working_tree_status", None, "git working-tree-status"),
        ("recent_changes", None, "git recent-changes"),
        ("commits_touching", None, "git commits-touching"),
        ("find_commits_by_path", None, "git find-commits-by-path"),
        ("diff_file", None, "git diff-file"),
        ("diff_outline", None, "git diff-outline"),
        ("hot_files", None, "git hot-files"),
        ("blame_file", None, "git blame-file"),
        ("blame_symbol", None, "git blame-symbol"),
        ("search_git_history", None, "git search"),
        ("memory_put", None, "memory put"),
        ("memory_get", None, "memory get"),
        ("memory_list", None, "memory list"),
        ("memory_search", None, "memory search"),
        ("memory_delete", None, "memory delete"),
        ("search_documents", None, "memory search-documents"),
        ("proposals_mine", None, "governance mine"),
        ("proposals_list", None, "governance proposals"),
        ("proposal_accept", None, "governance accept"),
        ("proposal_reject", None, "governance reject"),
        ("memory_audit", None, "governance audit"),
        ("cache_stats", None, "cache stats"),
        ("cache_gc", None, "cache gc"),
        ("cache_clear", None, "cache clear"),
    ];
    #[cfg(feature = "crawl")]
    m.extend([
        ("web", Some("scrape"), "web scrape"),
        ("web", Some("crawl"), "web crawl"),
        ("web", Some("map"), "web map"),
    ]);
    #[cfg(all(feature = "comms", any(unix, windows)))]
    m.extend([
        ("agent_register", None, "comms register"),
        ("agent_list", None, "comms agents"),
        ("thread_start", None, "comms thread-start"),
        ("thread_list", None, "comms threads"),
        ("thread_join", None, "comms join"),
        ("thread_leave", None, "comms leave"),
        ("thread_members", None, "comms members"),
        ("thread_add_member", None, "comms add-member"),
        ("thread_remove_member", None, "comms remove-member"),
        ("thread_archive", None, "comms archive"),
        ("thread_post", None, "comms post"),
        ("thread_history", None, "comms history"),
        ("message_get", None, "comms read"),
        ("inbox_read", None, "comms inbox"),
        ("inbox_ack", None, "comms inbox"),
        ("inbox_wait", None, "comms wait"),
        ("workspaces", None, "registry workspaces"),
        ("worktrees", None, "registry worktrees"),
        ("branches", None, "registry branches"),
        ("worktree_claim", None, "registry claim"),
        ("worktree_release", None, "registry release"),
    ]);
    #[cfg(all(feature = "shells", any(unix, windows)))]
    m.extend([
        ("shell_spawn", None, "shells spawn"),
        ("shell_send", None, "shells send"),
        ("shell_capture", None, "shells capture"),
        ("shell_kill", None, "shells kill"),
        ("shell_broadcast", None, "shells broadcast"),
        ("shell_list", None, "shells list"),
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
