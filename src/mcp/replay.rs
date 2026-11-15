//! Which `tools/call` requests the stdio relay may send a second time.
//!
//! When the daemon is replaced under a session, the relay re-sends what was in flight instead of
//! failing it, once and only when a second execution cannot change anything (ADR-0013). That is a
//! decision about each tool, and it has to be made when the tool is added: a tool nobody decided
//! on is failed rather than re-sent, which is safe and makes a read that could have survived a
//! restart fail for no reason, while a writing tool listed as safe by default would run twice.
//! Every tool the server advertises therefore appears in exactly one list below, and
//! `tests/mcp_smoke.rs` fails for one that appears in none.

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

/// Tools whose `mode` argument decides: the relay lists the modes it may re-send.
pub const REPLAY_BY_MODE_TOOLS: &[&str] = &["agents"];

/// How many of the lists name `tool`. Anything but one is a tool nobody decided on, or one
/// decided twice.
pub fn decisions(tool: &str) -> usize {
    [REPLAY_SAFE_TOOLS, NEVER_REPLAY_TOOLS, REPLAY_BY_MODE_TOOLS]
        .iter()
        .filter(|list| list.contains(&tool))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verb_tool_has_exactly_one_replay_decision() {
        for tool in crate::mcp::params::VERB_TOOLS {
            assert_eq!(decisions(tool), 1, "`{tool}` needs one replay decision");
        }
        assert_eq!(decisions("a_tool_nobody_listed"), 0);
    }
}
