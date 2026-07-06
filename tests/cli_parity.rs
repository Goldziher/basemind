//! Executable CLI↔MCP parity guard.
//!
//! basemind's contract is that every MCP `#[tool]` an agent can call over stdio is ALSO reachable
//! from the `basemind` CLI (agents and tests drive both). This test makes that contract enforceable:
//!
//! 1. It enumerates the live MCP tool surface from the in-process server
//!    ([`BasemindServer::tool_names`]) — the exact set `tools/list` advertises.
//! 2. It cross-references that set against `TOOL_TO_CLI`, a maintained table mapping each tool to
//!    the CLI command that invokes it.
//! 3. It asserts the mapping is a bijection (every tool mapped, every mapping real) and that each
//!    mapped CLI path actually resolves (`basemind <path> --help` exits 0).
//!
//! A new tool shipped without its CLI counterpart fails step 2 (unmapped tool); a renamed/removed
//! CLI command fails step 3. The table is feature-gated the same way the routers are, so the guard
//! is exact under whatever feature set the test is compiled with.

use std::process::Command;

use basemind::cli::context::build_server;
use basemind::config::DocumentsCliOverrides;
use basemind::store::VIEW_WORKING;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

/// The intended MCP-tool → CLI-command mapping. The CLI value is the argument path (sans the
/// `basemind` prefix and any positional operands) used to reach the identical tool code. Grouped by
/// router; feature-gated groups mirror the `#[cfg]` on their `tool_router_*` registration in
/// `src/mcp/mod.rs` so the set matches `tool_names()` exactly under any feature build.
fn tool_to_cli() -> Vec<(&'static str, &'static str)> {
    // `mut` is only exercised when a feature-gated `m.extend(...)` block below is compiled in;
    // on a default-feature build none of those blocks exist, so `mut` looks unused.
    #[allow(unused_mut)]
    let mut m: Vec<(&str, &str)> = vec![
        // code-map (tool_router_core)
        ("outline", "query outline"),
        ("search_symbols", "query search"),
        ("find_references", "query references"),
        ("find_callers", "query callers"),
        ("goto_definition", "query goto-definition"),
        ("find_implementations", "query implementations"),
        ("call_graph", "query call-graph"),
        ("architecture_map", "query architecture-map"),
        ("workspace_grep", "query grep"),
        ("list_files", "query list-files"),
        ("dependents", "query dependents"),
        ("status", "query status"),
        ("repo_info", "query repo-info"),
        ("symbol_history", "git symbol-history"),
        ("rescan", "rescan"),
        ("telemetry_summary", "telemetry"),
        // semantic code search (tool_router_code — shims compile without the feature)
        ("search_code", "query search-code"),
        ("get_chunk", "query get-chunk"),
        // code-aware compression (tool_router_compress)
        ("expand", "query expand"),
        ("compress", "compress-output"),
        ("delta", "delta"),
        ("checkpoint", "checkpoint"),
        ("detect_waste", "detect-waste"),
        // git history (tool_router_git)
        ("working_tree_status", "git working-tree-status"),
        ("recent_changes", "git recent-changes"),
        ("commits_touching", "git commits-touching"),
        ("find_commits_by_path", "git find-commits-by-path"),
        ("diff_file", "git diff-file"),
        ("diff_outline", "git diff-outline"),
        ("hot_files", "git hot-files"),
        ("blame_file", "git blame-file"),
        ("blame_symbol", "git blame-symbol"),
        ("search_git_history", "git search"),
        // shared memory (tool_router_memory — shims compile without the feature)
        ("memory_put", "memory put"),
        ("memory_get", "memory get"),
        ("memory_list", "memory list"),
        ("memory_search", "memory search"),
        ("memory_delete", "memory delete"),
        ("search_documents", "memory search-documents"),
        // governance (tool_router_governance)
        ("proposals_mine", "governance mine"),
        ("proposals_list", "governance proposals"),
        ("proposal_accept", "governance accept"),
        ("proposal_reject", "governance reject"),
        ("memory_audit", "governance audit"),
        // cache admin (tool_router_admin)
        ("cache_stats", "cache stats"),
        ("cache_gc", "cache gc"),
        ("cache_clear", "cache clear"),
    ];
    #[cfg(feature = "crawl")]
    m.extend([
        ("web_scrape", "web scrape"),
        ("web_crawl", "web crawl"),
        ("web_map", "web map"),
    ]);
    #[cfg(all(feature = "comms", any(unix, windows)))]
    m.extend([
        // Comms CLI verbs connect to the broker directly (a parallel path), but the capability of
        // every comms tool is reachable from the CLI. `inbox_ack` has no standalone verb — it is
        // folded into `comms inbox --mark-read`.
        ("agent_register", "comms register"),
        ("agent_list", "comms agents"),
        ("room_create", "comms room-create"),
        ("room_join", "comms join"),
        ("room_leave", "comms leave"),
        ("room_list", "comms rooms"),
        ("room_post", "comms post"),
        ("room_history", "comms history"),
        ("dm_send", "comms dm"),
        ("inbox_read", "comms inbox"),
        ("inbox_ack", "comms inbox"),
        ("message_get", "comms read"),
        ("get_or_create_chat_room_for_path", "comms room-for-path"),
    ]);
    #[cfg(all(feature = "shells", any(unix, windows)))]
    m.extend([
        ("shell_spawn", "shells spawn"),
        ("shell_send", "shells send"),
        ("shell_capture", "shells capture"),
        ("shell_kill", "shells kill"),
        ("shell_broadcast", "shells broadcast"),
        ("shell_list", "shells list"),
    ]);
    m
}

/// Build a one-shot server over an empty tempdir just to read its advertised tool set. The working
/// view opens read-only even when never scanned, so no fixture repo is needed.
fn advertised_tools() -> Vec<String> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server =
        build_server(tmp.path(), VIEW_WORKING, DocumentsCliOverrides::default()).expect("build one-shot server");
    server.tool_names()
}

#[test]
fn every_mcp_tool_has_a_cli_command() {
    let tools = advertised_tools();
    let map = tool_to_cli();
    let mapped: std::collections::HashSet<&str> = map.iter().map(|(t, _)| *t).collect();

    // 1. Every advertised MCP tool must be mapped to a CLI command. A new tool without an entry
    //    here fails — the author must add its CLI counterpart and record the mapping.
    let unmapped: Vec<&String> = tools.iter().filter(|t| !mapped.contains(t.as_str())).collect();
    assert!(
        unmapped.is_empty(),
        "MCP tools with no CLI mapping (add the CLI command + a TOOL_TO_CLI row): {unmapped:?}"
    );

    // 2. No stale mappings: every mapped tool must still be advertised (catches a renamed/removed
    //    tool whose stale row would otherwise mask a real gap).
    let live: std::collections::HashSet<&str> = tools.iter().map(String::as_str).collect();
    let stale: Vec<&str> = map.iter().map(|(t, _)| *t).filter(|t| !live.contains(t)).collect();
    assert!(
        stale.is_empty(),
        "TOOL_TO_CLI rows for tools no longer advertised (remove or rename them): {stale:?}"
    );
}

#[test]
fn every_mapped_cli_command_resolves() {
    // Each mapped CLI path must be a real, wired subcommand: `--help` parses the command tree and
    // exits 0 without running any tool. A renamed/missing CLI command fails here.
    for (tool, cli) in tool_to_cli() {
        let mut args: Vec<&str> = cli.split(' ').collect();
        args.push("--help");
        let output = Command::new(bin())
            .args(&args)
            .output()
            .unwrap_or_else(|e| panic!("spawn `basemind {cli} --help`: {e}"));
        assert!(
            output.status.success(),
            "`basemind {cli} --help` (for tool `{tool}`) exited {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
