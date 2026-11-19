//! The checked-in capability table: the single place that says which CLI command realises which
//! MCP operation, and which commands deliberately exist on the CLI only.
//!
//! # Keying
//!
//! Every row is one line keyed by plain strings: the MCP tool name and the CLI command path as typed
//! after `basemind`. Every tool is a verb tool now, so no row names a `mode`.
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
const BROKER: &str = "comms-broker daemon lifecycle; the MCP surface reaches the broker through the agent tools";
#[cfg(all(feature = "comms", any(unix, windows)))]
const HOOK: &str = "answers a plugin hook event on stdin; the hooks' entry, not an agent's";
#[cfg(all(feature = "comms", any(unix, windows)))]
const REGISTRY: &str = "read-only listing of the daemon's machine registry; the registry has no MCP tool";

/// Tools no session of the daemon advertises and no CLI command reaches, by design: they exist only
/// on the stdio of `basemind serve --lead` (ADR-0040). A CLI command would be a forwarded relay
/// session whose launcher identity is the peer's own claim, a second way in that review 9 must-fix 1
/// rules out, so these are the exception to "every MCP tool has a CLI command". `main.rs` checks
/// that this list is exactly `lead_tools::LEAD_TOOLS`, that no session advertises them and that no
/// CLI command is named after them.
pub const NOT_FORWARDED: &[(&str, &str)] = &[
    (
        "agent_launch",
        "answered only by `serve --lead` on its own stdio; never forwarded, so no CLI command reaches it",
    ),
    (
        "agent_stop",
        "answered only by `serve --lead` on its own stdio; never forwarded, so no CLI command reaches it",
    ),
];

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
        cli_only(
            "code impact",
            "an analysis answered by the daemon, no verb tool until an eval kind earns one (ADR-0047)",
        ),
        cli_only(
            "code cycles",
            "an analysis answered by the daemon, no verb tool until an eval kind earns one (ADR-0047)",
        ),
        cli_only(
            "code explain",
            "a briefing composed from outline, refs, the import graph and history; no verb tool until the explain \
             eval kind earns one (ADR-0047)",
        ),
        cli_only(
            "code migration check",
            "a report over repository rule files with a CI exit gate; no tool until an eval kind measures it (ADR-0047)",
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
            "git hotspots",
            "a whole-repository ranking with no verb tool until an eval kind promotes it (ADR-0047)",
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
            "manual",
            "prints the offline manual compiled into the binary; reads no index",
        ),
        cli_only(
            "admin tokens",
            "CLI-only benchmark primitive: prints a bare integer, no JSON envelope (`benchmarks/run.sh`)",
        ),
        cli_only(
            "sessions",
            "reads the operator's local agent transcripts in the CLI process; kept off MCP and the daemon (ADR-0049)",
        ),
        cli_only(
            "overview",
            "repository shape report; a CLI command answered by the daemon, no verb tool (ADR-0047)",
        ),
        cli_only(
            "admin eval",
            "offline evaluation harness over a JSONL task file; drives the tools rather than being one",
        ),
    ];
    #[cfg(all(feature = "comms", any(unix, windows)))]
    rows.extend([
        tool("agent_post", "agent post"),
        tool("agent_inbox", "agent inbox"),
        tool("agent_threads", "agent threads"),
        cli_only(
            "agent status",
            "reports who this checkout acts as and the daemon's agent store; an MCP session already knows",
        ),
        cli_only(
            "agent cleanup",
            "operator maintenance: archives any thread and sweeps retention, which no agent may do (ADR-0021)",
        ),
        cli_only("agent hook pre-tool-use", HOOK),
        cli_only("agent hook name", HOOK),
        cli_only("workspace workspaces", REGISTRY),
        cli_only("workspace worktrees", REGISTRY),
        cli_only("workspace branches", REGISTRY),
        cli_only("comms daemon", BROKER),
        cli_only("comms start", BROKER),
        cli_only("comms stop", BROKER),
        cli_only("comms status", BROKER),
        cli_only("comms doctor", BROKER),
        cli_only("comms cleanup", BROKER),
        cli_only(
            "daemon ensure",
            "prints the daemon's streamable-HTTP MCP URL for HTTP-native clients",
        ),
    ]);
    rows
}
