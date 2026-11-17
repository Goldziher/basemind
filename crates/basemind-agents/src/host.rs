//! Whether this process may host a supervisor.
//!
//! A supervisor makes its host the top of a launch tree: what it registers launches at depth 0,
//! under a budget, `max_concurrent` and `max_children` of its own. A `basemind serve` started
//! inside a launched session (the injected one, or the user's own MCP server that Codex and
//! OpenCode start from user-level configuration) would make the launched agent such a top, one
//! level down each time, and `max_depth` would bound nothing. So a process hosts a supervisor
//! only when it carries no launched session's variables and no ancestor of it runs the same
//! program: Codex filters the environment of the MCP servers it starts, so the variables alone
//! do not tell.

use crate::identity::{ENV_AGENT_DEPTH, ENV_LAUNCH_TOKEN};
use crate::procinfo;

/// Set in a test's own rerun of the test binary, whose parent then runs the same program: the
/// ancestry check is skipped there, the variables are still checked. Read in tests only.
#[cfg(test)]
pub(crate) const ALLOW_SAME_PROGRAM_ANCESTOR: &str = "BASEMIND_TEST_ALLOW_SAME_PROGRAM_ANCESTOR";

/// Why this process may not host a supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nested {
    /// It carries this variable of a launched session.
    Env(&'static str),
    /// The ancestor of this pid runs the same program.
    Ancestor(i32),
    /// Its ancestry, or what it runs, could not be read.
    Unknown,
}

/// Why this process may not host a supervisor, `None` when it may.
pub(crate) fn nested() -> Option<Nested> {
    for name in [ENV_LAUNCH_TOKEN, ENV_AGENT_DEPTH] {
        if std::env::var_os(name).is_some() {
            return Some(Nested::Env(name));
        }
    }
    #[cfg(test)]
    if std::env::var_os(ALLOW_SAME_PROGRAM_ANCESTOR).is_some() {
        return None;
    }
    let Ok(pid) = i32::try_from(std::process::id()) else {
        return Some(Nested::Unknown);
    };
    same_program_above(pid)
}

/// Whether an ancestor of `pid` runs the program `pid` runs. An ancestor whose program cannot be
/// read (another user's, on macOS) is passed over: the `serve` this guards against runs as the
/// user, below the lead's own.
fn same_program_above(pid: i32) -> Option<Nested> {
    let (Some(me), Some(mine)) = (procinfo::info(pid), procinfo::image(pid)) else {
        return Some(Nested::Unknown);
    };
    let Some(chain) = procinfo::ancestors(&me) else {
        return Some(Nested::Unknown);
    };
    for proc in &chain.procs {
        let Some(theirs) = procinfo::image(proc.pid) else {
            continue;
        };
        // What was read is the ancestor's only if the pid still names it.
        if procinfo::info(proc.pid).is_none_or(|now| now.start != proc.start) {
            return Some(Nested::Unknown);
        }
        if theirs.same_program(&mine) {
            return Some(Nested::Ancestor(proc.pid));
        }
    }
    None
}
