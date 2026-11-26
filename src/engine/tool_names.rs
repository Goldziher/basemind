//! The tool names the CLI and the MCP adapter both have to agree on, so neither imports the other.

/// Name of the agent tool that posts a message.
pub const AGENT_POST: &str = "agent_post";

/// Name of the agent tool that reads messages.
pub const AGENT_INBOX: &str = "agent_inbox";

/// Name of the agent tool that lists and changes threads.
pub const AGENT_THREADS: &str = "agent_threads";

/// The three agent tools, in declaration order. Deferred, so not part of the verb tools.
pub const AGENT_TOOLS: &[&str] = &[AGENT_POST, AGENT_INBOX, AGENT_THREADS];

/// The mode-dispatched tools the verb tools replaced (ADR-0018). None may be advertised.
pub const RETIRED_TOOLS: &[&str] = &[
    "code",
    "git",
    "memory",
    "admin",
    "graph",
    "web",
    "shell",
    "workspace",
    "agents",
];

/// The always-loaded verb tools that only read, in verb-tool declaration order. The MCP adapter
/// annotates each `readOnlyHint: true` and a test there keeps the two lists equal.
pub const READ_ONLY_VERB_TOOLS: &[&str] = &[
    "code_outline",
    "code_symbols",
    "code_read",
    "code_grep",
    "code_search",
    "code_refs",
    "find_files",
    "docs_search",
    "git_log",
    "git_blame",
    "git_diff",
    "memory_search",
    "index_status",
];
