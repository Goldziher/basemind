//! Who a caller is. The kernel's word on the peer process decides; what the caller claims (a
//! principal, a token) is only checked against it.
//!
//! Every harness leads a POSIX session of its own, so a process inside it cannot move into
//! another session's process group (`setpgid` across sessions is `EPERM`), and a process that
//! leaves with `setsid` still has the harness among its ancestors while its parent lives. A peer
//! is therefore attributed to a session by its session id, by its ancestry, or by the detached
//! processes the supervisor recorded for the session, never by its process group.

use crate::peer::Peer;
use crate::procinfo::{self, ProcInfo};
use crate::supervisor::{Resolved, Table};
use crate::supervisor_types::{Caller, CallerKind, SupervisorError};

/// Where a peer process sits relative to the supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// This process: the embedding's own code.
    Supervisor,
    /// Inside, or descended from, the live session of this id.
    Session(u64),
    /// Descended from this process, in no live session: what a finished session left behind,
    /// or a process the embedding started itself.
    Below,
    /// A process outside the supervisor's tree.
    Foreign,
    /// Gone, its pid reused, or its ancestry could not be read.
    Unknown,
}

/// Where `peer` sits now.
pub(crate) fn origin(table: &Table, peer: &Peer) -> Origin {
    let Some(me) = procinfo::info(peer.id.pid).filter(|me| me.start == peer.id.start && !me.zombie) else {
        return Origin::Unknown;
    };
    let supervisor = i32::try_from(std::process::id()).unwrap_or(-1);
    if me.pid == supervisor {
        return Origin::Supervisor;
    }
    let Some(ancestors) = procinfo::ancestors(me.pid) else {
        return Origin::Unknown;
    };
    for proc in std::iter::once(&me).chain(&ancestors) {
        if let Some(id) = table.session_of(proc) {
            return Origin::Session(id);
        }
        if proc.pid == supervisor {
            return Origin::Below;
        }
    }
    Origin::Foreign
}

/// Who `caller` is.
///
/// - A peer inside a live session is that session, whatever principal it claims; a token it
///   presents has to be that session's own, and it is never the operator.
/// - A peer below the supervisor in no live session is nobody.
/// - A peer outside the supervisor's tree (or the supervisor's own process) is the principal it
///   claims, when the policy admits it, or the operator; a token presented from outside every
///   session's processes is refused, since it was read from another process.
/// - A peer that is gone or cannot be placed is refused.
pub(crate) fn resolve(
    policy: &crate::LaunchPolicy,
    table: &Table,
    caller: &Caller,
) -> Result<Resolved, SupervisorError> {
    let Some(peer) = &caller.peer else {
        // Only an operator caller is built without a peer.
        return match caller.kind {
            CallerKind::Operator => Ok(Resolved::Operator),
            _ => Err(SupervisorError::PeerUnverified),
        };
    };
    match origin(table, peer) {
        Origin::Unknown => Err(SupervisorError::PeerUnverified),
        Origin::Below => Err(SupervisorError::CallerNotAdmitted),
        Origin::Session(id) => match &caller.kind {
            CallerKind::Token(token) if !table.sessions[&id].token.matches(token) => Err(token_refusal(table, token)),
            CallerKind::Operator => Err(SupervisorError::CallerNotAdmitted),
            _ => Ok(Resolved::Session(id)),
        },
        Origin::Supervisor | Origin::Foreign => match &caller.kind {
            CallerKind::Operator => Ok(Resolved::Operator),
            CallerKind::Token(token) => Err(token_refusal(table, token)),
            CallerKind::Principal(principal) if policy.admits(principal) => Ok(Resolved::Top(principal.clone())),
            CallerKind::Principal(_) => Err(SupervisorError::CallerNotAdmitted),
        },
    }
}

/// A token presented from a process outside its session was read from that session's
/// environment; one that names no live session is simply not valid.
fn token_refusal(table: &Table, token: &str) -> SupervisorError {
    if table.live().any(|(_, entry)| entry.token.matches(token)) {
        SupervisorError::PeerMismatch
    } else {
        SupervisorError::UnknownToken
    }
}

impl Table {
    /// The live session `proc` belongs to: it is the session's harness, a member of the POSIX
    /// session the harness leads, or a detached process recorded for it.
    pub(crate) fn session_of(&self, proc: &ProcInfo) -> Option<u64> {
        self.live()
            .find(|(_, entry)| {
                entry.leader.is_some_and(|leader| {
                    proc.id() == leader
                        // A session id is the leader's pid; a later process holding that pid
                        // started after it.
                        || (proc.sid == leader.pid && proc.start >= leader.start)
                })
            })
            .map(|(id, _)| *id)
    }
}
