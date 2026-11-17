//! Who a caller is. The kernel's word on the peer process decides; what the caller claims (a
//! principal, a token) is only checked against it. Nothing is trusted by default.
//!
//! Every harness leads a POSIX session of its own, so a process inside it cannot move into
//! another session's process group (`setpgid` across sessions is `EPERM`), and a process that
//! leaves with `setsid` still has the harness among its ancestors while its parent lives. A peer
//! is therefore attributed to a session by its session id, by its ancestry, or by the detached
//! processes the supervisor recorded for the session, never by its process group.
//!
//! A configured launcher and the operator are each bound by the engine to one process
//! ([`Supervisor::register_launcher`](crate::Supervisor::register_launcher),
//! [`Supervisor::register_operator`](crate::Supervisor::register_operator)). A caller is that
//! principal only when its peer is that process or a descendant of it, read link by link with
//! each parent no younger than its child, and the walk failing closed. A peer in no live
//! session and under no registered process is refused, whatever it claims: a launched agent's
//! process that escaped its session (orphaned, double-forked, or started for it by tmux,
//! launchd or ssh) descends from init or from that spawner, never from the registered process.

use crate::peer::Peer;
use crate::procinfo::{self, ProcId, ProcInfo};
use crate::supervisor::{Resolved, Table};
use crate::supervisor_types::{Caller, CallerKind, SupervisorError};

/// What a registered process may act as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Launcher(String),
    Operator,
}

/// Where a peer process sits relative to the supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// This process, not registered.
    Supervisor,
    /// Inside, or descended from, the live session of this id.
    Session(u64),
    /// Is, or descends from, these registered processes (nearest first), in no live session.
    Registered(Vec<ProcId>),
    /// Descended from this process, in no live session and under no registered process: what
    /// a finished session left behind, or a process the embedding started itself.
    Below,
    /// Outside the supervisor's tree, under no registered process. `complete` when its
    /// ancestry was read up to init; otherwise it stopped at a process that could not be read.
    Outside { complete: bool },
    /// Gone, its pid reused, or a link of its ancestry vanished while it was read.
    Unknown,
}

/// Where `peer` sits now.
pub(crate) fn origin(table: &Table, peer: &Peer) -> Origin {
    let Some(me) = procinfo::info(peer.id.pid).filter(|me| me.start == peer.id.start && !me.zombie) else {
        return Origin::Unknown;
    };
    let supervisor = i32::try_from(std::process::id()).unwrap_or(-1);
    let Some(chain) = procinfo::ancestors(me.pid) else {
        return Origin::Unknown;
    };
    let mut registered = Vec::new();
    for proc in std::iter::once(&me).chain(&chain.procs) {
        if let Some(id) = table.session_of(proc) {
            return Origin::Session(id);
        }
        if proc.pid == supervisor {
            // The supervisor's own process is registered only as itself: what descends from it
            // is a session's or a leftover, never the principal it was registered as.
            if proc.id() == me.id() {
                return if table.is_registered(me.id()) {
                    Origin::Registered(vec![me.id()])
                } else {
                    Origin::Supervisor
                };
            }
            return if registered.is_empty() {
                Origin::Below
            } else {
                Origin::Registered(registered)
            };
        }
        if table.is_registered(proc.id()) {
            registered.push(proc.id());
        }
    }
    if registered.is_empty() {
        Origin::Outside {
            complete: chain.complete,
        }
    } else {
        Origin::Registered(registered)
    }
}

/// Who `caller` is.
///
/// - A peer inside a live session is that session, whatever principal it claims; a token it
///   presents has to be that session's own, and it is never the operator.
/// - A peer that is, or descends from, the process registered for a principal is that
///   principal when it claims it and the policy admits it; the operator likewise.
/// - A peer below the supervisor in no live session is nobody.
/// - Any other peer, and one that is gone or cannot be placed, is refused as unverified.
pub(crate) fn resolve(
    policy: &crate::LaunchPolicy,
    table: &Table,
    caller: &Caller,
) -> Result<Resolved, SupervisorError> {
    match origin(table, &caller.peer) {
        Origin::Unknown | Origin::Outside { .. } | Origin::Supervisor => Err(SupervisorError::PeerUnverified),
        Origin::Below => Err(SupervisorError::CallerNotAdmitted),
        Origin::Session(id) => match &caller.kind {
            CallerKind::Token(token) if !table.sessions[&id].token.matches(token) => Err(token_refusal(table, token)),
            CallerKind::Operator => Err(SupervisorError::CallerNotAdmitted),
            _ => Ok(Resolved::Session(id)),
        },
        Origin::Registered(ids) => match &caller.kind {
            CallerKind::Operator if table.registered_as(&ids, &Role::Operator) => Ok(Resolved::Operator),
            CallerKind::Token(token) => Err(token_refusal(table, token)),
            CallerKind::Principal(principal)
                if policy.admits(principal) && table.registered_as(&ids, &Role::Launcher(principal.clone())) =>
            {
                Ok(Resolved::Top(principal.clone()))
            }
            CallerKind::Operator | CallerKind::Principal(_) => Err(SupervisorError::CallerNotAdmitted),
        },
    }
}

/// The key a refusal of an unverified caller on `peer` is coalesced under: where the peer
/// sits, never what it claims, so one process cannot spread its refusals over many keys.
pub(crate) fn coalescing_key(table: &Table, peer: &Peer) -> String {
    match origin(table, peer) {
        Origin::Session(id) => format!("session-{id}"),
        Origin::Registered(ids) => format!("registered-{}", ids.first().map_or(0, |id| id.pid)),
        Origin::Below => "below".to_owned(),
        Origin::Supervisor => "supervisor".to_owned(),
        Origin::Outside { .. } | Origin::Unknown => "unverified".to_owned(),
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

/// The operator caller for `peer`, when `peer` is, or descends from, the registered operator
/// process.
pub(crate) fn mint_operator(table: &Table, peer: Peer) -> Result<Caller, SupervisorError> {
    match origin(table, &peer) {
        Origin::Registered(ids) if table.registered_as(&ids, &Role::Operator) => Ok(Caller {
            kind: CallerKind::Operator,
            peer,
        }),
        Origin::Unknown | Origin::Outside { .. } | Origin::Supervisor => Err(SupervisorError::PeerUnverified),
        Origin::Registered(_) | Origin::Session(_) | Origin::Below => Err(SupervisorError::CallerNotAdmitted),
    }
}

/// Binds `role` to `peer`'s process, replacing the process it was bound to. The process must not
/// be a session's. One the embedding started itself is accepted, and so is one whose ancestry
/// stops at a process that cannot be read: the engine vouches for the process it registers.
pub(crate) fn register(
    policy: &crate::LaunchPolicy,
    table: &mut Table,
    role: Role,
    peer: Peer,
) -> Result<(), SupervisorError> {
    if let Role::Launcher(principal) = &role
        && !policy.admits(principal)
    {
        return Err(SupervisorError::CallerNotAdmitted);
    }
    match origin(table, &peer) {
        Origin::Unknown => Err(SupervisorError::PeerUnverified),
        Origin::Session(_) => Err(SupervisorError::CallerNotAdmitted),
        Origin::Supervisor | Origin::Below | Origin::Outside { .. } | Origin::Registered(_) => {
            table.registered.retain(|(bound, _)| *bound != role);
            table.registered.push((role, peer.id));
            Ok(())
        }
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
                }) || entry.detached.contains(proc.id())
            })
            .map(|(id, _)| *id)
    }

    fn is_registered(&self, id: ProcId) -> bool {
        self.registered.iter().any(|(_, bound)| *bound == id)
    }

    /// Whether one of `ids` is bound to `role`.
    fn registered_as(&self, ids: &[ProcId], role: &Role) -> bool {
        self.registered
            .iter()
            .any(|(bound_role, bound)| bound_role == role && ids.contains(bound))
    }
}
