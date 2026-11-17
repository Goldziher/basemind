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
//!
//! A process bound as a launcher or the operator in-process takes requests on its own stdio (the
//! lead's tool calls). On Linux any process of the same user may open `/proc/<pid>/fd/0` of a
//! dumpable process (`PTRACE_MODE_READ`, which Yama does not restrict) and write whole requests
//! into it, so such a process is made non-dumpable first ([`seal`]): its `/proc` entries become
//! root's, and `ptrace` needs `CAP_SYS_PTRACE`. An embedding seals itself as the first thing it
//! does ([`seal_host`]), so no request can be written into its stdio between its start and its
//! registration.

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

/// The command name a sealed host gives itself on Linux ([`seal_host`]). Its executable cannot be
/// read once it is non-dumpable, so a `serve` started below it by a launched agent's harness
/// recognises it by this name, whatever name the lead's own binary was started under.
pub const HOST_MARKER: &str = "basemind-lead";

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
        if theirs.same_program(&mine) || theirs.comm.as_deref() == Some(HOST_MARKER.as_bytes()) {
            return Some(Nested::Ancestor(proc.pid));
        }
    }
    None
}

/// Seals this process as the host of a supervisor, before it reads anything: on Linux it is made
/// non-dumpable ([`seal`]) and renamed to [`HOST_MARKER`], so a `basemind serve` started below it
/// is refused as nested even though this process's executable can no longer be read. Returns
/// whether the process is non-dumpable now (always `true` off Linux, where there is no `/proc` to
/// open another process's descriptors through).
///
/// An embedding that hosts a supervisor calls this as its first statement: the registration
/// seals too, but only after configuration was read, and a process of the same user that was
/// already waiting could open `/proc/<pid>/fd/0` for writing in between. A short window from
/// `exec` to this call remains.
#[must_use]
pub fn seal_host() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let name = std::ffi::CString::new(HOST_MARKER).expect("the marker holds no NUL");
        // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most 16 bytes, which `name` is.
        unsafe {
            libc::prctl(libc::PR_SET_NAME, name.as_ptr(), 0, 0, 0);
        }
    }
    seal()
}

/// Makes this process non-dumpable where that guards its descriptors (Linux): returns whether
/// it is now. A no-op elsewhere: macOS has no `/proc` to open another process's descriptors
/// through, and launching is Unix-only.
///
/// The flag is reset when the process executes another program, so the harnesses this process
/// starts are dumpable as usual. Core dumps of this process are off from then on.
pub(crate) fn seal() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: prctl with PR_SET_DUMPABLE and PR_GET_DUMPABLE takes plain integers.
        unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
            libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) == 0
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        true
    }
}
