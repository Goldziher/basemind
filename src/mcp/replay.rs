//! Which `tools/call` requests the stdio relay may send a second time.
//!
//! When the daemon is replaced under a session, the relay re-sends what was in flight instead of
//! failing it, once and only when a second execution cannot change anything (ADR-0013). That is a
//! decision about each tool, and it has to be made when the tool is added: a tool nobody decided
//! on is failed rather than re-sent, which is safe and makes a read that could have survived a
//! restart fail for no reason, while a writing tool listed as safe by default would run twice.
//! Every tool the server advertises therefore appears in exactly one list below, and
//! `tests/mcp_smoke.rs` fails for one that appears in none.

/// `_meta` key under which the stdio relay stamps an agent tool's call with its replay key
/// (ADR-0021): never an argument of the tool.
pub const REPLAY_KEY_META: &str = "basemind/replay_key";

/// Tools that only read. Re-sending one is harmless.
pub const REPLAY_SAFE_TOOLS: &[&str] = &[
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

/// Tools that write, or start work, each time they run. Never re-sent.
pub const NEVER_REPLAY_TOOLS: &[&str] = &["memory_save", "index_rescan"];

/// Tools the relay stamps with a replay key, in the request's `_meta`, and may re-send with the
/// same key: the daemon answers a replay with the first result (ADR-0021). A post is stored once,
/// a marking read returns the ranges it delivered, a thread change finds itself made, and a read
/// that marks nothing is a read.
pub const KEYED_REPLAY_TOOLS: &[&str] = &["agent_post", "agent_inbox", "agent_threads"];

/// How many of the lists name `tool`. Anything but one is a tool nobody decided on, or one
/// decided twice.
pub fn decisions(tool: &str) -> usize {
    [REPLAY_SAFE_TOOLS, NEVER_REPLAY_TOOLS, KEYED_REPLAY_TOOLS]
        .iter()
        .filter(|list| list.contains(&tool))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_tool_has_exactly_one_replay_decision() {
        let tools = crate::mcp::params::VERB_TOOLS
            .iter()
            .chain(crate::mcp::agent_tools::AGENT_TOOLS);
        for tool in tools {
            assert_eq!(decisions(tool), 1, "`{tool}` needs one replay decision");
        }
        assert_eq!(decisions("a_tool_nobody_listed"), 0);
    }
}
