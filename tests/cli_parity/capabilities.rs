//! The checked-in capability table: the single place that says which CLI command realises which
//! MCP operation, and which operations deliberately exist on one side only.
//!
//! # Keying
//!
//! Every row is one line keyed by plain strings — the MCP `(tool, mode)` wire spelling and the CLI
//! command path as typed after `basemind`. Renaming or moving a CLI command is therefore a one-line
//! edit to the `cli` string of its row; adding a command or mode means adding one row. The coverage
//! test (`main.rs`) fails with the exact row to add or delete.
//!
//! # Row kinds
//!
//! * [`pair`] — an MCP `tool`+`mode` and the CLI command that invokes the identical operation.
//! * [`cli_only`] — a CLI command with no MCP counterpart; a non-empty `reason` is required.
//! * [`mcp_only`] — an MCP mode with no CLI command; a non-empty `reason` is required.
//!
//! Rows for feature-gated surfaces sit behind the same `#[cfg]` as the routers/subcommands they
//! describe, so the table is exact for whatever feature set the test is built with.

/// An MCP operation: `tool` is the domain tool name, `mode` its `mode` enum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct McpOp {
    pub tool: &'static str,
    pub mode: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub enum Row {
    Pair { mcp: McpOp, cli: &'static str },
    CliOnly { cli: &'static str, reason: &'static str },
    McpOnly { mcp: McpOp, reason: &'static str },
}

impl Row {
    pub fn mcp(&self) -> Option<McpOp> {
        match self {
            Row::Pair { mcp, .. } | Row::McpOnly { mcp, .. } => Some(*mcp),
            Row::CliOnly { .. } => None,
        }
    }

    pub fn cli(&self) -> Option<&'static str> {
        match self {
            Row::Pair { cli, .. } | Row::CliOnly { cli, .. } => Some(cli),
            Row::McpOnly { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Row::CliOnly { reason, .. } | Row::McpOnly { reason, .. } => Some(reason),
            Row::Pair { .. } => None,
        }
    }
}

const fn pair(tool: &'static str, mode: &'static str, cli: &'static str) -> Row {
    Row::Pair {
        mcp: McpOp { tool, mode },
        cli,
    }
}

const fn cli_only(cli: &'static str, reason: &'static str) -> Row {
    Row::CliOnly { cli, reason }
}

#[allow(dead_code)] // no MCP-only mode exists today; kept so adding one is a one-line row
const fn mcp_only(tool: &'static str, mode: &'static str, reason: &'static str) -> Row {
    Row::McpOnly {
        mcp: McpOp { tool, mode },
        reason,
    }
}

/// The reasons below are shared by several rows.
const LIFECYCLE: &str = "process/index lifecycle, not a query an agent issues over MCP";
const BROKER: &str = "comms-broker daemon lifecycle; the MCP surface reaches the broker through `agents`/`workspace`";
#[cfg(not(feature = "crawl"))]
const NO_CRAWL: &str =
    "the MCP `web` tool is compiled out without the `crawl` feature; the CLI reports the missing feature";

pub fn table() -> Vec<Row> {
    #[allow(unused_mut)]
    let mut rows = vec![
        // ---- code ----
        pair("code", "outline", "code outline"),
        pair("code", "symbols", "code symbols"),
        pair("code", "grep", "code grep"),
        pair("code", "files", "code files"),
        pair("code", "find", "code find"),
        pair("code", "definition", "code definition"),
        pair("code", "references", "code references"),
        pair("code", "callers", "code callers"),
        pair("code", "implementations", "code implementations"),
        pair("code", "dependents", "code dependents"),
        pair("code", "expand", "code expand"),
        pair("code", "semantic", "code semantic"),
        pair("code", "chunk", "code chunk"),
        // ---- graph ----
        pair("graph", "calls", "graph calls"),
        pair("graph", "neighbors", "graph neighbors"),
        pair("graph", "path", "graph path"),
        pair("graph", "subgraph", "graph subgraph"),
        pair("graph", "communities", "graph communities"),
        pair("graph", "map", "graph map"),
        pair("graph", "export", "graph export"),
        pair("graph", "display", "graph display"),
        pair("graph", "open", "graph open"),
        // ---- admin ----
        pair("admin", "status", "status"),
        pair("admin", "repo", "admin repo"),
        pair("admin", "rescan", "rescan"),
        pair("admin", "cache_stats", "cache stats"),
        pair("admin", "gc", "cache gc"),
        pair("admin", "cache_clear", "cache clear"),
        pair("admin", "telemetry", "admin telemetry"),
        pair("admin", "compress", "admin compress"),
        pair("admin", "delta", "delta"),
        pair("admin", "checkpoint", "checkpoint"),
        pair("admin", "waste", "detect-waste"),
        // ---- git ----
        pair("git", "status", "git status"),
        pair("git", "recent", "git recent"),
        pair("git", "touching", "git touching"),
        pair("git", "by_path", "git by-path"),
        pair("git", "churn", "git churn"),
        pair("git", "diff", "git diff"),
        pair("git", "diff_outline", "git diff-outline"),
        pair("git", "blame", "git blame"),
        pair("git", "blame_symbol", "git blame-symbol"),
        pair("git", "symbol_history", "git symbol-history"),
        pair("git", "search", "git search"),
        // ---- memory ----
        pair("memory", "put", "memory put"),
        pair("memory", "get", "memory get"),
        pair("memory", "list", "memory list"),
        pair("memory", "search", "memory search"),
        pair("memory", "delete", "memory delete"),
        pair("memory", "audit", "memory audit"),
        pair("memory", "documents", "memory documents"),
        pair("memory", "mine", "memory mine"),
        pair("memory", "proposals", "memory proposals"),
        pair("memory", "accept", "memory accept"),
        pair("memory", "reject", "memory reject"),
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
            "compress-output",
            "hook filter: family-detected compression of command output read from stdin, failing open to raw passthrough; `admin compress` takes an explicit level/target instead",
        ),
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
    #[cfg(feature = "crawl")]
    rows.extend([
        pair("web", "scrape", "web scrape"),
        pair("web", "crawl", "web crawl"),
        pair("web", "map", "web map"),
    ]);
    // The `web` CLI group is always compiled in; its MCP tool only exists with `crawl`.
    #[cfg(not(feature = "crawl"))]
    rows.extend([
        cli_only("web scrape", NO_CRAWL),
        cli_only("web crawl", NO_CRAWL),
        cli_only("web map", NO_CRAWL),
    ]);
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
    #[cfg(all(feature = "shells", any(unix, windows)))]
    rows.extend([
        pair("shell", "spawn", "shell spawn"),
        pair("shell", "send", "shell send"),
        pair("shell", "capture", "shell capture"),
        pair("shell", "kill", "shell kill"),
        pair("shell", "list", "shell list"),
        pair("shell", "broadcast", "shell broadcast"),
    ]);
    rows
}
