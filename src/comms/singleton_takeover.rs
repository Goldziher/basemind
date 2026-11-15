//! What a client does about a daemon already running in its generation.
//!
//! A child of [`singleton`](super), which states the decision table in its module doc. The rule
//! the code here exists to keep: a daemon that answers is never signalled. Signals are for a
//! process that has gone silent and that this generation's `daemon.pid` still names.

use std::time::{Duration, Instant};

use super::super::protocol::StatusReport;
use super::super::store::STORE_DIR;
use super::probe::{StopOutcome, force_terminate, request_stop_classified};
use super::{
    CommsPaths, NO_AUTOSPAWN_ENV, SPAWN_POLL_INTERVAL, SingletonError, SpawnPolicy, autospawn_disabled, daemon_status,
    off_worker, probe_alive, version_is_older,
};
use crate::daemon_lock::{self, DaemonKind};

/// The store error token: the one `Stop` refusal that reports a fault rather than a decision.
const STORE_ERROR: &str = "store_error";

/// The waits a takeover is allowed. Injected so the tests do not sit through production graces.
#[derive(Clone, Copy, Debug)]
pub(super) struct Timings {
    /// How long a daemon asked to stop has to stop answering `Ping`.
    pub(super) drain: Duration,
    /// How long a daemon that closed its socket has to exit on its own.
    pub(super) exit: Duration,
    /// How long a silent daemon gets on SIGTERM before SIGKILL.
    pub(super) term_grace: Duration,
}

impl Timings {
    /// `exit` covers the daemon's own 10s drain grace, so a clean drain is never signalled.
    pub(super) const PRODUCTION: Self = Self {
        drain: Duration::from_secs(3),
        exit: Duration::from_secs(12),
        term_grace: Duration::from_secs(5),
    };
}

/// This binary's side of the comparison. A parameter rather than the crate constants so the tests
/// can stand on either side of a running daemon.
#[derive(Clone, Copy, Debug)]
pub(super) struct Ours {
    /// This binary's `MAJOR.MINOR.PATCH`.
    pub(super) version: &'static str,
    /// This binary's protocol version.
    pub(super) proto_ver: u32,
}

/// Whether a call may ask an older daemon to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Takeover {
    /// Ask an older daemon to stop so this binary's can replace it.
    Ask,
    /// Contact nothing but `Status`: for a caller that retries after its `Stop` was turned down,
    /// so the daemon is not sent another `Stop` on every attempt. An older daemon is used when it
    /// speaks this binary's protocol and reported as [`SingletonError::StalePredecessor`] otherwise.
    Wait,
}

/// What the caller should do once the running daemon, if any, has been dealt with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Existing {
    /// A daemon this binary can talk to is running.
    Use,
    /// Nothing is serving this generation; spawn.
    Spawn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Use,
    RefuseNewer,
    /// Ask it to stop. `usable` is whether this binary can fall back to it when it declines.
    Replace {
        usable: bool,
    },
}

fn classify(report: &StatusReport, ours: Ours) -> Verdict {
    let same_proto = report.proto_ver == ours.proto_ver;
    if version_is_older(&report.version, ours.version) {
        Verdict::Replace { usable: same_proto }
    } else if same_proto || !version_is_older(ours.version, &report.version) {
        // The same release on another protocol does not occur between releases; the handshake
        // reports it rather than this binary stopping a daemon that is not older than itself.
        Verdict::Use
    } else {
        Verdict::RefuseNewer
    }
}

/// Apply the decision table to whatever is running at `paths`.
pub(super) async fn settle_existing(
    paths: &CommsPaths,
    policy: SpawnPolicy,
    takeover: Takeover,
    ours: Ours,
    timings: Timings,
) -> Result<Existing, SingletonError> {
    let socket = paths.socket_path.as_path();
    let Some(report) = off_worker(|| daemon_status(socket)) else {
        return Ok(Existing::Spawn);
    };
    let usable = match classify(&report, ours) {
        Verdict::Use => return Ok(Existing::Use),
        Verdict::RefuseNewer => {
            return Err(SingletonError::NewerDaemon {
                version: report.version,
                pid: report.pid,
                daemon_proto: report.proto_ver,
                ours: ours.version.to_string(),
                our_proto: ours.proto_ver,
            });
        }
        Verdict::Replace { usable } => usable,
    };
    // Never stop a daemon we are then forbidden to replace. Under connect-only the takeover
    // would leave the operator with no daemon at all — strictly worse than an old one, whose
    // skew the handshake reports precisely.
    if policy == SpawnPolicy::Auto && autospawn_disabled() {
        tracing::warn!(
            daemon_version = %report.version,
            daemon_pid = report.pid,
            ours = ours.version,
            env = NO_AUTOSPAWN_ENV,
            "comms: a daemon from another build holds the socket, but auto-spawn is disabled; using it as-is"
        );
        return Ok(Existing::Use);
    }
    if takeover == Takeover::Wait {
        return if usable { Ok(Existing::Use) } else { Err(stale(report)) };
    }
    tracing::info!(
        daemon_version = %report.version,
        daemon_pid = report.pid,
        ours = ours.version,
        "comms: a daemon from another build holds the socket; asking it to stop"
    );
    match off_worker(|| request_stop_classified(socket)) {
        StopOutcome::Refused { code, message } if code == STORE_ERROR => {
            return Err(SingletonError::FailingPredecessor {
                version: report.version,
                pid: report.pid,
                store: paths.comms_dir.join(STORE_DIR),
                message,
            });
        }
        StopOutcome::Refused { code, message } if usable => {
            tracing::info!(
                daemon_version = %report.version,
                daemon_pid = report.pid,
                code,
                message,
                "comms: the running daemon declined to stop and speaks our protocol; using it"
            );
            return Ok(Existing::Use);
        }
        StopOutcome::Refused { code, message } => {
            return Err(SingletonError::BusyPredecessor {
                version: report.version,
                pid: report.pid,
                code,
                message,
            });
        }
        StopOutcome::Accepted | StopOutcome::Unreachable => {}
    }
    if !wait_until(timings.drain, || !off_worker(|| probe_alive(socket))).await {
        return Err(stale(report));
    }
    reclaim_silent(paths, report, timings).await
}

/// The socket has gone quiet. A daemon that closed its socket is usually on its way out — draining
/// after this client's `Stop`, after another client's, or on its own idle exit — so it gets the
/// whole exit window whether or not it answered this `Stop`. Only a process still there at the end
/// of the window is signalled, and only if the endpoint is still silent then and this generation's
/// pidfile, read at that moment, names the pid the daemon itself reported. A pid taken from the wire
/// alone could be anything: an in-process test broker reports its host.
async fn reclaim_silent(
    paths: &CommsPaths,
    report: StatusReport,
    timings: Timings,
) -> Result<Existing, SingletonError> {
    let pid = report.pid;
    if wait_until(timings.exit, || !daemon_lock::pid_is_live(pid)).await {
        return Ok(Existing::Spawn);
    }
    if off_worker(|| probe_alive(&paths.socket_path)) {
        return Err(stale(report));
    }
    let named = daemon_lock::recorded_holder(&paths.comms_dir)
        .is_some_and(|holder| holder.pid == pid && holder.kind == DaemonKind::Comms);
    if !named {
        // Alive, silent and not the recorded holder: it may not be signalled, and a daemon spawned
        // now would only exit on the lock that process may still hold.
        return Err(stale(report));
    }
    tracing::warn!(
        daemon_version = %report.version,
        daemon_pid = pid,
        "comms: the previous daemon stopped answering without exiting; terminating it by pid"
    );
    if off_worker(|| force_terminate(pid, timings.term_grace)) {
        Ok(Existing::Spawn)
    } else {
        Err(stale(report))
    }
}

fn stale(report: StatusReport) -> SingletonError {
    SingletonError::StalePredecessor {
        version: report.version,
        pid: report.pid,
    }
}

/// Poll `done` until it holds or `limit` elapses; the final verdict is always a fresh check.
async fn wait_until(limit: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
    }
}

#[cfg(all(test, unix))]
#[path = "singleton_takeover_tests.rs"]
mod tests;
