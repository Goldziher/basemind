//! The checked-in capability table: the single place that says which CLI command realises which
//! MCP operation, and which commands deliberately exist on the CLI only.
//!
//! # Keying
//!
//! Every row is one line keyed by plain strings: the MCP tool name (plus its `mode`, for the two
//! domain tools that are still mode-dispatched) and the CLI command path as typed after `basemind`.
//! Renaming or moving a CLI command is a one-line edit to the `cli` string of its row; adding a
//! command or tool means adding one row. The coverage test (`main.rs`) fails with the exact row to
//! add or delete.
//!
//! # Row kinds
//!
//! * [`pair`] — an MCP tool and THE CLI command that runs the identical body with the same
//!   parameters. Exactly one per tool (ADR-0018: every MCP tool has a CLI command; the mapping is
//!   injective, not a bijection).
//! * [`shorthand`] — a further CLI command that runs the same tool body with some arguments
//!   pre-set (`git status` is `git diff` with no arguments). Its arguments must still be the
//!   tool's.
//! * [`cli_only`] — a CLI command with no MCP tool; a non-empty `reason` is required.
//! * [`mcp_only`] — an MCP operation with no CLI command; a non-empty `reason` is required.
//!
//! Rows for feature-gated surfaces sit behind the same `#[cfg]` as the routers/subcommands they
//! describe, so the table is exact for whatever feature set the test is built with.

/// An MCP operation: `tool` is the tool name; `mode` is its `mode` enum value for a mode-dispatched
/// domain tool and empty for a verb tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct McpOp {
    pub tool: &'static str,
    pub mode: &'static str,
}

impl McpOp {
    pub fn label(&self) -> String {
        if self.mode.is_empty() {
            self.tool.to_string()
        } else {
            format!("{}:{}", self.tool, self.mode)
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Row {
    Pair {
        mcp: McpOp,
        cli: &'static str,
    },
    Shorthand {
        mcp: McpOp,
        cli: &'static str,
        reason: &'static str,
    },
    CliOnly {
        cli: &'static str,
        reason: &'static str,
    },
    McpOnly {
        mcp: McpOp,
        reason: &'static str,
    },
}

impl Row {
    /// The MCP operation this row accounts for on the coverage side: its canonical pairing, or its
    /// declared absence from the CLI. A shorthand accounts for no operation.
    pub fn mcp(&self) -> Option<McpOp> {
        match self {
            Row::Pair { mcp, .. } | Row::McpOnly { mcp, .. } => Some(*mcp),
            Row::Shorthand { .. } | Row::CliOnly { .. } => None,
        }
    }

    pub fn cli(&self) -> Option<&'static str> {
        match self {
            Row::Pair { cli, .. } | Row::Shorthand { cli, .. } | Row::CliOnly { cli, .. } => Some(cli),
            Row::McpOnly { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Row::Shorthand { reason, .. } | Row::CliOnly { reason, .. } | Row::McpOnly { reason, .. } => Some(reason),
            Row::Pair { .. } => None,
        }
    }

    /// The tool whose body this CLI command runs, for pairs and shorthands.
    pub fn runs(&self) -> Option<(McpOp, &'static str)> {
        match self {
            Row::Pair { mcp, cli } | Row::Shorthand { mcp, cli, .. } => Some((*mcp, cli)),
            Row::CliOnly { .. } | Row::McpOnly { .. } => None,
        }
    }
}

/// A verb tool and its CLI command.
const fn tool(tool: &'static str, cli: &'static str) -> Row {
    Row::Pair {
        mcp: McpOp { tool, mode: "" },
        cli,
    }
}

/// One mode of a mode-dispatched domain tool and its CLI command.
#[cfg(all(feature = "comms", any(unix, windows)))]
const fn pair(tool: &'static str, mode: &'static str, cli: &'static str) -> Row {
    Row::Pair {
        mcp: McpOp { tool, mode },
        cli,
    }
}

const fn shorthand(tool: &'static str, cli: &'static str, reason: &'static str) -> Row {
    Row::Shorthand {
        mcp: McpOp { tool, mode: "" },
        cli,
        reason,
    }
}

const fn cli_only(cli: &'static str, reason: &'static str) -> Row {
    Row::CliOnly { cli, reason }
}

#[allow(dead_code)] // no MCP-only operation exists today; kept so adding one is a one-line row
const fn mcp_only(tool: &'static str, mode: &'static str, reason: &'static str) -> Row {
    Row::McpOnly {
        mcp: McpOp { tool, mode },
        reason,
    }
}

/// The reasons below are shared by several rows.
const LIFECYCLE: &str = "process/index lifecycle, not a query an agent issues over MCP";
const CACHE_ADMIN: &str = "machine-global cache administration; kept off the agent surface (ADR-0018) and run offline";
#[cfg(all(feature = "comms", any(unix, windows)))]
const BROKER: &str = "comms-broker daemon lifecycle; the MCP surface reaches the broker through `agents`/`workspace`";

pub fn table() -> Vec<Row> {
    #[allow(unused_mut)]
    let mut rows = vec![
        // ---- code ----
        tool("code_outline", "code outline"),
        tool("code_symbols", "code symbols"),
        tool("code_read", "code read"),
        tool("code_grep", "code grep"),
        tool("code_search", "code search"),
        tool("code_refs", "code refs"),
        tool("find_files", "code find"),
        shorthand("code_refs", "code callers", "`refs <name> --path <path>`"),
        shorthand(
            "code_refs",
            "code implementations",
            "`refs <name> --relation implementations`",
        ),
        shorthand("code_refs", "code dependents", "`refs <module> --relation importers`"),
        shorthand(
            "code_refs",
            "code definition",
            "`refs <name> --relation definition --path <path> --line <line>`",
        ),
        shorthand("find_files", "code files", "`find` with no query"),
        cli_only(
            "code chunk",
            "fetches one semantic-search chunk by id; over MCP a hit's line span is read with `code_read`",
        ),
        // ---- documents and memory ----
        tool("docs_search", "memory documents"),
        tool("memory_save", "memory put"),
        tool("memory_search", "memory search"),
        shorthand("memory_search", "memory get", "the `key` shape of `memory_search`"),
        shorthand(
            "memory_search",
            "memory list",
            "the `prefix` / list-all shape of `memory_search`",
        ),
        cli_only(
            "memory delete",
            "deleting shared memory is kept off the agent surface (ADR-0018)",
        ),
        // ---- git ----
        tool("git_log", "git log"),
        tool("git_blame", "git blame"),
        tool("git_diff", "git diff"),
        shorthand("git_log", "git recent", "`log` with no filter"),
        shorthand("git_log", "git touching", "`log --path <path>`"),
        shorthand("git_log", "git search", "`log --query <query>`"),
        shorthand("git_log", "git show", "`log --rev <rev>`"),
        shorthand("git_log", "git symbol-history", "`log --path <path> --symbol <name>`"),
        shorthand("git_blame", "git blame-symbol", "`blame <path> --symbol <name>`"),
        shorthand("git_diff", "git status", "`diff` with no arguments"),
        shorthand(
            "git_diff",
            "git diff-outline",
            "`diff <path> --structural --from <rev>`",
        ),
        cli_only(
            "git churn",
            "churn ranking measured no use over MCP and was left off the verb surface (ADR-0018)",
        ),
        cli_only(
            "git by-path",
            "arbitrary path-regex commit log; `git_log` covers a file or directory `path`",
        ),
        // ---- index ----
        tool("index_status", "status"),
        tool("index_rescan", "rescan"),
        shorthand(
            "index_status",
            "admin repo",
            "the status payload carries branch and HEAD",
        ),
        cli_only("cache stats", CACHE_ADMIN),
        cli_only("cache gc", CACHE_ADMIN),
        cli_only("cache clear", CACHE_ADMIN),
        cli_only(
            "admin telemetry",
            "usage summary for a human dashboard; kept off the agent surface (ADR-0018)",
        ),
        // ---- CLI-only: indexing, hooks, grammars, serving ----
        cli_only(
            "init",
            "one-time repo onboarding (writes basemind.toml and rules files); not an agent query",
        ),
        cli_only("scan", LIFECYCLE),
        cli_only("watch", LIFECYCLE),
        cli_only("serve", "starts the MCP server itself"),
        cli_only("statusline", "shell-statusline helper reading the daemon registry"),
        cli_only(
            "hook install",
            "installs a git pre-commit hook; filesystem setup, not a query",
        ),
        cli_only("lang list", "tree-sitter grammar cache management"),
        cli_only("lang install", "tree-sitter grammar cache management"),
        cli_only("lang clean", "tree-sitter grammar cache management"),
        cli_only(
            "doctor",
            "installation and workspace health check printing human diagnostics and exiting 1 on failure; an MCP client is already connected",
        ),
        cli_only(
            "completions",
            "emits a shell completion script for the CLI's own grammar",
        ),
        cli_only("man", "emits the CLI man page (roff)"),
        cli_only(
            "admin tokens",
            "CLI-only benchmark primitive: prints a bare integer, no JSON envelope (`benchmarks/run.sh`)",
        ),
        cli_only(
            "admin eval",
            "offline evaluation harness over a JSONL task file; drives the tools rather than being one",
        ),
    ];
    #[cfg(all(feature = "comms", any(unix, windows)))]
    rows.extend([
        pair("agents", "register", "agents register"),
        pair("agents", "list", "agents list"),
        pair("agents", "thread_start", "agents thread-start"),
        pair("agents", "thread_list", "agents thread-list"),
        pair("agents", "join", "agents join"),
        pair("agents", "leave", "agents leave"),
        pair("agents", "members", "agents members"),
        pair("agents", "add_member", "agents add-member"),
        pair("agents", "remove_member", "agents remove-member"),
        pair("agents", "archive", "agents archive"),
        pair("agents", "post", "agents post"),
        pair("agents", "history", "agents history"),
        pair("agents", "message", "agents message"),
        pair("agents", "inbox", "agents inbox"),
        pair("agents", "ack", "agents ack"),
        pair("agents", "wait", "agents wait"),
        pair("agents", "cleanup", "agents cleanup"),
        pair("agents", "status", "agents status"),
        pair("workspace", "workspaces", "workspace workspaces"),
        pair("workspace", "worktrees", "workspace worktrees"),
        pair("workspace", "branches", "workspace branches"),
        pair("workspace", "claim", "workspace claim"),
        pair("workspace", "release", "workspace release"),
        cli_only("comms daemon", BROKER),
        cli_only("comms start", BROKER),
        cli_only("comms stop", BROKER),
        cli_only("comms status", BROKER),
        cli_only("comms doctor", BROKER),
        cli_only(
            "daemon ensure",
            "prints the daemon's streamable-HTTP MCP URL for HTTP-native clients",
        ),
    ]);
    rows
}
