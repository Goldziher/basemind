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
#[allow(dead_code)]
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

const REPEATABLE: &str = "repeatable CLI flag for the MCP list";
const TIER: &str = "CLI switch selects the `individual` tier; MCP takes the tier name";
const SYMBOL: &str = "the shorthand's positional `name` is the tool's `symbol`";

pub fn all() -> Vec<Exception> {
    vec![
        // ---- renamed arguments ----
        alias("memory put", "tag", "tags", REPEATABLE),
        alias("memory put", "individual", "visibility", TIER),
        alias("memory search", "individual", "visibility", TIER),
        alias("memory get", "individual", "visibility", TIER),
        alias("memory list", "individual", "visibility", TIER),
        alias(
            "code dependents",
            "module",
            "name",
            "the shorthand's positional `module` is the tool's `name`",
        ),
        alias("git symbol-history", "name", "symbol", SYMBOL),
        alias("git blame-symbol", "name", "symbol", SYMBOL),
        alias(
            "git diff-outline",
            "rev",
            "from",
            "the shorthand's `--rev` is the revision the working tree is compared against",
        ),
        alias("rescan", "path", "paths", "variadic positional for the MCP list"),
        // ---- CLI-only arguments ----
        cli_only(
            "agent inbox",
            "announce",
            "the hooks' announcing read; not a field of `agent_inbox` (ADR-0021)",
        ),
        cli_only(
            "rescan",
            "no_git_history",
            "TODO parity: `index_rescan` cannot skip the git-history index build",
        ),
        cli_only(
            "rescan",
            "rebuild_git_history",
            "TODO parity: `index_rescan` cannot wipe and rebuild the git-history index",
        ),
        // ---- MCP-only parameters ----
        mcp_only(
            "memory search",
            "key",
            "the `key` shape of `memory_search` is `basemind memory get`",
        ),
        mcp_only(
            "memory search",
            "prefix",
            "the `prefix` shape of `memory_search` is `basemind memory list`",
        ),
        mcp_only(
            "memory search",
            "cursor",
            "only a listing pages; `basemind memory list` takes `--cursor`",
        ),
        // ---- required-ness ----
        required(
            "memory search",
            "query",
            "`basemind memory search` is the query shape of `memory_search`, whose selector is optional",
        ),
    ]
}
