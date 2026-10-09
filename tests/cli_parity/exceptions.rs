//! Reviewed parameter-parity waivers (layer 2) and the global-flag declarations.
//!
//! Each line says why a CLI argument and an MCP field do not line up. Keep reasons honest: a
//! `TODO parity:` reason is a known gap to close, anything else is an intentional difference.

use crate::params::{Diff, Exception};

const fn cli_only(cli: &'static str, param: &'static str, reason: &'static str) -> Exception {
    Exception {
        cli,
        param,
        diff: Diff::CliOnly,
        reason,
    }
}
const fn mcp_only(cli: &'static str, param: &'static str, reason: &'static str) -> Exception {
    Exception {
        cli,
        param,
        diff: Diff::McpOnly,
        reason,
    }
}
#[allow(dead_code)]
const fn alias(cli: &'static str, param: &'static str, mcp: &'static str, reason: &'static str) -> Exception {
    Exception {
        cli,
        param,
        diff: Diff::Alias(mcp),
        reason,
    }
}
#[allow(dead_code)]
const fn shape(cli: &'static str, param: &'static str, reason: &'static str) -> Exception {
    Exception {
        cli,
        param,
        diff: Diff::Shape,
        reason,
    }
}
#[allow(dead_code)]
const fn values(cli: &'static str, param: &'static str, reason: &'static str) -> Exception {
    Exception {
        cli,
        param,
        diff: Diff::Values,
        reason,
    }
}
#[allow(dead_code)]
const fn default(cli: &'static str, param: &'static str, reason: &'static str) -> Exception {
    Exception {
        cli,
        param,
        diff: Diff::Default,
        reason,
    }
}
#[allow(dead_code)]
const fn required(cli: &'static str, param: &'static str, reason: &'static str) -> Exception {
    Exception {
        cli,
        param,
        diff: Diff::Required,
        reason,
    }
}

/// Flags declared once on the root command and inherited by every subcommand. Each must be listed
/// with the reason it has no per-mode MCP field.
pub const GLOBAL_FLAGS: &[(&str, &str)] = &[
    (
        "root",
        "selects the repository; an MCP server is bound to its root at startup",
    ),
    ("quiet", "terminal verbosity; MCP has no console"),
    ("verbose", "terminal verbosity; MCP has no console"),
    ("no_color", "terminal colouring; MCP responses are JSON"),
    ("json", "CLI output switch; MCP responses are always JSON"),
    (
        "view",
        "selects the index view; an MCP server is bound to one view at startup",
    ),
];

const GRAPH_KNOBS: &str =
    "TODO parity: graph traversal knob not exposed on the CLI; the command runs with the MCP default";
const INVERTED: &str = "CLI spells the negation (`--no-X`) of the MCP boolean `X`";
const STDIN: &str = "CLI reads this from stdin, MCP takes it as a required parameter";

pub fn all() -> Vec<Exception> {
    vec![
        // ---- renamed / sense-inverted arguments ----
        alias("code grep", "no_context", "include_context", INVERTED),
        alias("git recent", "no_files", "include_files", INVERTED),
        alias("admin compress", "no_preserve_code", "preserve_code", INVERTED),
        alias("memory put", "no_embed", "embed", INVERTED),
        alias("web scrape", "no_index", "index", INVERTED),
        alias("shell send", "no_enter", "enter", INVERTED),
        alias("shell broadcast", "no_enter", "enter", INVERTED),
        alias(
            "shell broadcast",
            "session",
            "session_ids",
            "repeatable CLI flag for the MCP id list",
        ),
        alias("agents post", "tag", "tags", "repeatable CLI flag for the MCP list"),
        alias(
            "agents register",
            "skill",
            "skills",
            "repeatable CLI flag for the MCP list",
        ),
        alias("rescan", "path", "paths", "variadic positional for the MCP list"),
        alias("graph map", "no_churn", "include_churn", INVERTED),
        alias(
            "cache clear",
            "yes",
            "confirm",
            "CLI `--yes` skips the interactive prompt; MCP has no terminal, so the caller confirms with `confirm`",
        ),
        alias(
            "memory *",
            "individual",
            "visibility",
            "CLI switch selects the `individual` tier; MCP takes the tier name",
        ),
        // ---- CLI-only arguments ----
        cli_only(
            "agents cleanup",
            "dry_run",
            "no-op flag (preview is the default); MCP expresses intent through `apply`",
        ),
        cli_only(
            "cache gc",
            "dry_run",
            "the CLI deletes by default and `--dry-run` selects the report; the MCP `gc` mode is always the read-only report",
        ),
        cli_only(
            "rescan",
            "no_git_history",
            "TODO parity: the MCP `rescan` cannot skip the git-history index build",
        ),
        cli_only(
            "rescan",
            "rebuild_git_history",
            "TODO parity: the MCP `rescan` cannot wipe and rebuild the git-history index",
        ),
        // ---- MCP-only parameters ----
        mcp_only("checkpoint", "text", STDIN),
        mcp_only("graph communities", "max_communities", GRAPH_KNOBS),
        mcp_only("graph communities", "members_per_community", GRAPH_KNOBS),
        mcp_only("graph export", "algorithm", GRAPH_KNOBS),
        mcp_only("graph export", "edges", GRAPH_KNOBS),
        mcp_only("graph export", "max_edges", GRAPH_KNOBS),
        mcp_only("graph export", "max_nodes", GRAPH_KNOBS),
        mcp_only("graph export", "min_confidence", GRAPH_KNOBS),
        mcp_only(
            "graph export",
            "write",
            "TODO parity: MCP can write the export to a file; the CLI prints it",
        ),
        mcp_only("graph neighbors", "depth", GRAPH_KNOBS),
        mcp_only("graph neighbors", "direction", GRAPH_KNOBS),
        mcp_only("graph neighbors", "edges", GRAPH_KNOBS),
        mcp_only("graph neighbors", "max_nodes", GRAPH_KNOBS),
        mcp_only("graph neighbors", "min_confidence", GRAPH_KNOBS),
        mcp_only("graph path", "edges", GRAPH_KNOBS),
        mcp_only("graph path", "include_contains", GRAPH_KNOBS),
        mcp_only("graph path", "min_confidence", GRAPH_KNOBS),
        mcp_only("graph path", "to_path", GRAPH_KNOBS),
        mcp_only("graph subgraph", "depth", GRAPH_KNOBS),
        mcp_only("graph subgraph", "edges", GRAPH_KNOBS),
        mcp_only("graph subgraph", "max_nodes", GRAPH_KNOBS),
        mcp_only("graph subgraph", "min_confidence", GRAPH_KNOBS),
        // ---- allowed values ----
        values(
            "graph calls",
            "direction",
            "the MCP `direction` field is shared with `neighbors` (`both|out|in`) and also accepts `callers`/`callees` synonyms there, so no single enum fits",
        ),
        values(
            "graph display",
            "format",
            "the MCP `format` enum is the `export` set; `display` accepts only the visual subset `html|svg`",
        ),
        values(
            "graph open",
            "format",
            "the MCP `format` enum is the `export` set; `open` accepts only the visual subset `html|svg`",
        ),
        // ---- required-ness ----
        required(
            "cache clear",
            "component",
            "TODO parity: MCP requires `component`; the CLI defaults to `git-cache`",
        ),
        required("delta", "new", STDIN),
        required("detect-waste", "log", STDIN),
    ]
}
