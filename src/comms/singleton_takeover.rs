//! What a client does about a daemon already running in its generation.
//!
//! A child of [`singleton`](super), which states the decision table in its module doc. The rule
//! the code here exists to keep: a daemon that answers is never signalled, and neither is one that
//! could not be asked. Signals are for a process that was asked and stayed silent, while this
//! generation's `daemon.lock` reads held and its `daemon.pid` names that process.
//!
//! The decisions reach the machine only through [`Generation`], so the same code is tested on
//! every platform against a scripted one (`singleton_takeover_decision_tests.rs`), and on Unix
//! against real sockets and processes (`singleton_takeover_tests.rs`).

use std::path::Path;
use std::time::{Duration, Instant};

use super::super::protocol::StatusReport;
use super::super::store::STORE_DIR;
use super::probe::{StopOutcome, force_terminate, request_stop_within};
use super::{
    CommsPaths, ENDPOINT_TIMEOUT, Liveness, NO_AUTOSPAWN_ENV, SPAWN_POLL_INTERVAL, SingletonError, SpawnPolicy,
    autospawn_disabled, daemon_status, endpoint_accepts, off_worker, probe_liveness, version_is_older,
};
use crate::daemon_lock::{self, DaemonKind, DaemonRecord, LockState};

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

    /// A takeover reclaims by the strictest rules: any answer at all spares the daemon.
    fn reclaim(self) -> ReclaimRules {
        ReclaimRules {
            exit: self.exit,
            term_grace: self.term_grace,
            probe_timeout: ENDPOINT_TIMEOUT,
            store_error_is_silence: false,
        }
    }
}

/// How a caller reclaims a generation from a daemon that has stopped answering.
#[derive(Clone, Copy, Debug)]
pub struct ReclaimRules {
    /// How long the daemon has to leave by itself before anything else is tried.
    pub exit: Duration,
    /// How long it gets on SIGTERM before SIGKILL.
    pub term_grace: Duration,
    /// How long the `Stop` sent at the end of the exit window waits for an answer.
    pub probe_timeout: Duration,
    /// Whether a `store_error` answer to that `Stop` counts as no answer. It does for an operator's
    /// explicit stop: a daemon that answers only to say its store is broken is the one they are
    /// trying to get rid of. It never does for a takeover.
    pub store_error_is_silence: bool,
}

/// How [`reclaim_holder`] left the generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reclaimed {
    /// No daemon is there any more. `forced` says it took a signal.
    Gone {
        /// Whether the daemon had to be terminated by pid.
        forced: bool,
    },
    /// The daemon answered at the end of the exit window and was left alone. `stop` is its answer
    /// to `Stop`; [`StopOutcome::Unreachable`] there means it answered `Ping` only.
    Answers {
        /// What it answered to the end-of-window `Stop`.
        stop: StopOutcome,
    },
    /// Something is still there and answers nothing, but it cannot be tied to this generation's
    /// daemon: `daemon.pid` does not name a live comms daemon (or not the expected one), or
    /// `daemon.lock` is free, so whoever the pidfile names does not hold the generation. Nothing
    /// was signalled.
    Unidentified,
    /// The process `daemon.pid` names was signalled and is still alive.
    Survived {
        /// The pid that outlived SIGKILL.
        pid: u32,
    },
    /// The daemon did not answer `Stop` and could not be asked whether it is alive: the `Ping`
    /// was never sent. That is not silence, so nothing was signalled.
    Unknown,
}

/// What the takeover and the reclaim need from the machine: the daemon's endpoint, the
/// generation's lock and pidfile, and the one way a process is signalled.
pub(super) trait Generation {
    /// The generation's directory, for naming its store in an error.
    fn dir(&self) -> &Path;
    /// The daemon's answer to `Status`, if it gave one.
    fn status(&self) -> Option<StatusReport>;
    /// Ask the daemon to stop, waiting up to `timeout` for its answer.
    fn stop(&self, timeout: Duration) -> StopOutcome;
    /// Whether the daemon answers `Ping`.
    fn ping(&self) -> Liveness;
    /// Whether anything accepts a connection at the endpoint.
    fn accepts(&self) -> bool;
    /// What `daemon.lock` says.
    fn lock_state(&self) -> LockState;
    /// Whether a process runs under `pid`.
    fn pid_is_live(&self, pid: u32) -> bool;
    /// The live process `daemon.pid` names.
    fn recorded_holder(&self) -> Option<DaemonRecord>;
    /// Whether the process now under `holder.pid` can be the daemon that wrote `holder`.
    fn can_have_written(&self, holder: &DaemonRecord) -> bool;
    /// Signal `pid` to terminate; whether it is gone afterwards.
    fn terminate(&self, pid: u32, grace: Duration) -> bool;
}

/// The machine as it is: the generation at `paths`.
struct Live<'a>(&'a CommsPaths);

impl Generation for Live<'_> {
    fn dir(&self) -> &Path {
        &self.0.comms_dir
    }

    fn status(&self) -> Option<StatusReport> {
        off_worker(|| daemon_status(&self.0.socket_path))
    }

    fn stop(&self, timeout: Duration) -> StopOutcome {
        off_worker(|| request_stop_within(&self.0.socket_path, timeout))
    }

    fn ping(&self) -> Liveness {
        off_worker(|| probe_liveness(&self.0.socket_path))
    }

    fn accepts(&self) -> bool {
        endpoint_accepts(&self.0.socket_path)
    }

    fn lock_state(&self) -> LockState {
        daemon_lock::lock_state(&self.0.comms_dir)
    }

    fn pid_is_live(&self, pid: u32) -> bool {
        daemon_lock::pid_is_live(pid)
    }

    fn recorded_holder(&self) -> Option<DaemonRecord> {
        daemon_lock::recorded_holder(&self.0.comms_dir)
    }

    fn can_have_written(&self, holder: &DaemonRecord) -> bool {
        crate::process_start::can_have_written(holder)
    }

    fn terminate(&self, pid: u32, grace: Duration) -> bool {
        off_worker(|| force_terminate(pid, grace))
    }
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

/// Apply the decision table to whatever is running at `paths`. `stop_sent` is set once a `Stop`
/// has been sent, whatever came of it.
pub(super) async fn settle_existing(
    paths: &CommsPaths,
    policy: SpawnPolicy,
    takeover: Takeover,
    ours: Ours,
    timings: Timings,
    stop_sent: &mut bool,
) -> Result<Existing, SingletonError> {
    settle(&Live(paths), policy, takeover, ours, timings, stop_sent).await
}

/// [`settle_existing`] over any [`Generation`]. [`Existing::Spawn`] here means only that no
/// daemon answered `Status`; the caller still pings before it spawns.
async fn settle(
    generation: &impl Generation,
    policy: SpawnPolicy,
    takeover: Takeover,
    ours: Ours,
    timings: Timings,
    stop_sent: &mut bool,
) -> Result<Existing, SingletonError> {
    let Some(report) = generation.status() else {
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
    *stop_sent = true;
    match generation.stop(ENDPOINT_TIMEOUT) {
        StopOutcome::Refused { code, message } if code == STORE_ERROR => {
            return Err(SingletonError::FailingPredecessor {
                version: report.version,
                pid: report.pid,
                store: generation.dir().join(STORE_DIR),
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
    // Only a daemon that was asked and stayed silent has stopped answering. One that could not
    // be asked is treated as still there.
    if !wait_until(timings.drain, || generation.ping() == Liveness::Silent).await {
        return Err(stale(report));
    }
    match reclaim(generation, Some(report.pid), timings.reclaim()).await {
        Reclaimed::Gone { .. } => Ok(Existing::Spawn),
        // Alive and answering, alive and not the recorded holder, or not known to be silent: it
        // may not be signalled, and a daemon spawned now would only exit on the lock that process
        // may still hold.
        Reclaimed::Answers { .. } | Reclaimed::Unidentified | Reclaimed::Survived { .. } | Reclaimed::Unknown => {
            Err(stale(report))
        }
    }
}

/// Reclaim a generation from a daemon that has stopped answering: the one path by which a daemon
/// is ever signalled, shared by the takeover and by `basemind comms stop` (`--all` included).
///
/// A daemon that closed its socket is usually on its way out — draining after this caller's `Stop`,
/// after another's, or on its own idle exit — so first it gets the whole `rules.exit` window. Gone
/// then means `reported_pid` no longer runs, or, when the caller has no pid from the daemon itself,
/// that nothing holds `daemon.lock` and nothing accepts a connection at the endpoint. A daemon still
/// there at the end of the window is asked again, `Stop` and then `Ping`, and is left alone if it
/// answers either, or if the `Ping` could not be sent ([`Reclaimed::Unknown`]). Only one silent to
/// both is signalled, only while `daemon.lock` still reads held at that moment, and only the
/// process this generation's `daemon.pid` names then; given a `reported_pid`, the two must agree.
/// With the lock free nothing is signalled: the generation is reported gone when its endpoint is
/// closed too, whatever now runs under `reported_pid`, and unidentified when something still
/// accepts there. A pid taken from the wire alone could be anything: an
/// in-process test broker reports its host. And a pid, reported or recorded, outlives its process:
/// a daemon that died inside the exit window leaves both naming a number the system may have given
/// to something else, whereas its lock went with it.
///
/// The lock names nobody, though, and it can read held with a stale pidfile beside it: a new
/// daemon holds it before it writes its record, and another process testing it holds it for an
/// instant. So the process the pidfile names must also be no younger than the record (where the
/// platform tells a process's start time: macOS, Linux and Windows), and the lock is read again
/// after the pidfile. What remains is a process that already ran when the dead daemon took the
/// lock and was given its pid afterwards, which pid allocation makes unlikely and nothing here
/// rules out.
pub async fn reclaim_holder(paths: &CommsPaths, reported_pid: Option<u32>, rules: ReclaimRules) -> Reclaimed {
    reclaim(&Live(paths), reported_pid, rules).await
}

/// [`reclaim_holder`] over any [`Generation`].
async fn reclaim(generation: &impl Generation, reported_pid: Option<u32>, rules: ReclaimRules) -> Reclaimed {
    let gone = || match reported_pid {
        Some(pid) => !generation.pid_is_live(pid),
        None => generation.lock_state() != LockState::Held && !generation.accepts(),
    };
    if wait_until(rules.exit, &gone).await {
        return Reclaimed::Gone { forced: false };
    }
    let stop = generation.stop(rules.probe_timeout);
    let silent = match &stop {
        StopOutcome::Unreachable => match generation.ping() {
            Liveness::Silent => true,
            Liveness::Answers => false,
            Liveness::Unknown => return Reclaimed::Unknown,
        },
        StopOutcome::Refused { code, .. } => rules.store_error_is_silence && code == STORE_ERROR,
        StopOutcome::Accepted => false,
    };
    if !silent {
        return Reclaimed::Answers { stop };
    }
    if gone() {
        return Reclaimed::Gone { forced: false };
    }
    if generation.lock_state() != LockState::Held {
        // No daemon holds this generation. Whatever the pids name now is not it.
        return if generation.accepts() {
            Reclaimed::Unidentified
        } else {
            Reclaimed::Gone { forced: false }
        };
    }
    let Some(holder) = generation
        .recorded_holder()
        .filter(|holder| holder.kind == DaemonKind::Comms && reported_pid.is_none_or(|pid| holder.pid == pid))
        .filter(|holder| generation.can_have_written(holder))
    else {
        return Reclaimed::Unidentified;
    };
    // The lock was held a moment ago by someone; it must still be once the pidfile has named
    // the process, or that record is what a daemon left behind.
    if generation.lock_state() != LockState::Held {
        return Reclaimed::Unidentified;
    }
    tracing::warn!(
        daemon_version = %holder.version,
        daemon_pid = holder.pid,
        "comms: the daemon stopped answering without exiting; terminating it by pid"
    );
    if generation.terminate(holder.pid, rules.term_grace) {
        Reclaimed::Gone { forced: true }
    } else {
        Reclaimed::Survived { pid: holder.pid }
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

#[cfg(test)]
#[path = "singleton_takeover_decision_tests.rs"]
mod decision_tests;
