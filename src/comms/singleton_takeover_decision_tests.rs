//! The takeover and reclaim decisions against a scripted [`Generation`]. No socket, pipe or
//! process is involved, so these run on every platform: on Windows they are what holds the rule
//! that a daemon which answers, or could not be asked, is never signalled.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::comms::protocol::PROTO_VER;

const DAEMON_PID: u32 = 4242;
const OURS: Ours = Ours {
    version: "0.50.0",
    proto_ver: PROTO_VER,
};
const QUICK: Timings = Timings {
    drain: Duration::from_millis(20),
    exit: Duration::from_millis(20),
    term_grace: Duration::from_millis(20),
};

/// A generation whose daemon behaves as scripted. `asked` records what reached it, in order.
struct Scripted {
    dir: PathBuf,
    /// The version it reports to `Status`; `None` when it does not answer `Status`.
    version: Option<&'static str>,
    stop: StopOutcome,
    ping: Liveness,
    accepts: bool,
    lock: LockState,
    process_live: bool,
    holder: Option<u32>,
    /// Whether the process under the recorded pid is old enough to have written the record.
    holder_predates_record: bool,
    /// What the lock reads once the pidfile has been read, when that differs from `lock`.
    lock_after_pidfile: Option<LockState>,
    pidfile_read: Cell<bool>,
    asked: RefCell<Vec<&'static str>>,
    signalled: Cell<Option<u32>>,
}

impl Scripted {
    /// An older daemon that holds the generation, is recorded as its holder, and answers nothing.
    fn silent_holder() -> Self {
        Self {
            dir: PathBuf::from("comms/g1"),
            version: Some("0.49.0"),
            stop: StopOutcome::Unreachable,
            ping: Liveness::Silent,
            accepts: true,
            lock: LockState::Held,
            process_live: true,
            holder: Some(DAEMON_PID),
            holder_predates_record: true,
            lock_after_pidfile: None,
            pidfile_read: Cell::new(false),
            asked: RefCell::default(),
            signalled: Cell::new(None),
        }
    }

    fn asked(&self) -> Vec<&'static str> {
        self.asked.borrow().clone()
    }
}

impl Generation for Scripted {
    fn dir(&self) -> &Path {
        &self.dir
    }

    fn status(&self) -> Option<StatusReport> {
        self.asked.borrow_mut().push("status");
        self.version.map(|version| StatusReport {
            pid: DAEMON_PID,
            version: version.to_string(),
            build_id: String::new(),
            proto_ver: PROTO_VER + 1,
            uptime_secs: 1,
            threads: 0,
            subscribers: 0,
        })
    }

    fn stop(&self, _timeout: Duration) -> StopOutcome {
        self.asked.borrow_mut().push("stop");
        self.stop.clone()
    }

    fn ping(&self) -> Liveness {
        self.asked.borrow_mut().push("ping");
        self.ping
    }

    fn accepts(&self) -> bool {
        self.accepts
    }

    fn lock_state(&self) -> LockState {
        match self.lock_after_pidfile {
            Some(later) if self.pidfile_read.get() => later,
            _ => self.lock,
        }
    }

    fn can_have_written(&self, _holder: &DaemonRecord) -> bool {
        self.holder_predates_record
    }

    fn pid_is_live(&self, _pid: u32) -> bool {
        self.process_live && self.signalled.get().is_none()
    }

    fn recorded_holder(&self) -> Option<DaemonRecord> {
        self.pidfile_read.set(true);
        self.holder.map(|pid| DaemonRecord {
            pid,
            kind: DaemonKind::Comms,
            dir: self.dir.clone(),
            version: "0.49.0".to_string(),
            started_unix: 1_000,
            process_start: None,
            boot: None,
        })
    }

    fn terminate(&self, pid: u32, _grace: Duration) -> bool {
        self.signalled.set(Some(pid));
        true
    }
}

fn operator() -> ReclaimRules {
    ReclaimRules {
        exit: Duration::from_millis(20),
        term_grace: Duration::from_millis(20),
        probe_timeout: Duration::from_millis(20),
        store_error_is_silence: false,
    }
}

async fn take_over(generation: &Scripted) -> Result<Existing, SingletonError> {
    settle(
        generation,
        SpawnPolicy::Explicit,
        Takeover::Ask,
        OURS,
        QUICK,
        &mut false,
    )
    .await
}

/// The one case that ends in a signal: asked `Stop`, asked `Ping`, silent to both, lock held, and
/// the pidfile names the process.
#[tokio::test]
async fn a_daemon_silent_to_stop_and_ping_that_holds_the_lock_is_signalled() {
    for reported in [Some(DAEMON_PID), None] {
        let generation = Scripted::silent_holder();
        assert_eq!(
            reclaim(&generation, reported, operator()).await,
            Reclaimed::Gone { forced: true }
        );
        assert_eq!(generation.signalled.get(), Some(DAEMON_PID));
        assert_eq!(
            generation.asked(),
            ["stop", "ping"],
            "it was asked both before the signal"
        );
    }
}

/// The rule itself: whatever else is true, a daemon that answers `Ping` is left alone.
#[tokio::test]
async fn a_daemon_that_answers_ping_is_never_signalled() {
    let generation = Scripted {
        ping: Liveness::Answers,
        ..Scripted::silent_holder()
    };
    assert_eq!(
        reclaim(&generation, Some(DAEMON_PID), operator()).await,
        Reclaimed::Answers {
            stop: StopOutcome::Unreachable
        }
    );
    assert_eq!(generation.signalled.get(), None);

    let taken_over = take_over(&generation).await;
    assert!(
        matches!(
            taken_over,
            Err(SingletonError::StalePredecessor { pid: DAEMON_PID, .. })
        ),
        "{taken_over:?}"
    );
    assert_eq!(generation.signalled.get(), None);
}

/// A `Ping` that was never sent is not an unanswered one. The daemon may be answering every other
/// client, so it is not signalled, by a reclaim or by a takeover, and the takeover does not go on
/// to spawn a second daemon either.
#[tokio::test]
async fn a_daemon_that_could_not_be_asked_is_neither_signalled_nor_replaced() {
    let generation = Scripted {
        ping: Liveness::Unknown,
        ..Scripted::silent_holder()
    };
    assert_eq!(
        reclaim(&generation, Some(DAEMON_PID), operator()).await,
        Reclaimed::Unknown
    );
    assert_eq!(reclaim(&generation, None, operator()).await, Reclaimed::Unknown);
    assert_eq!(generation.signalled.get(), None);

    let taken_over = take_over(&generation).await;
    assert!(
        matches!(
            taken_over,
            Err(SingletonError::StalePredecessor { pid: DAEMON_PID, .. })
        ),
        "not `Existing::Spawn`: {taken_over:?}"
    );
    assert_eq!(generation.signalled.get(), None);
}

/// An answer to `Stop` spares the daemon, a refusal included. Only an operator's stop counts a
/// `store_error` answer as silence.
#[tokio::test]
async fn a_daemon_that_answers_stop_is_not_signalled_unless_it_only_reports_a_broken_store() {
    let refusal = |code: &str| StopOutcome::Refused {
        code: code.to_string(),
        message: "no".to_string(),
    };
    for stop in [StopOutcome::Accepted, refusal("daemon_busy"), refusal(STORE_ERROR)] {
        let generation = Scripted {
            stop: stop.clone(),
            ..Scripted::silent_holder()
        };
        assert_eq!(
            reclaim(&generation, None, operator()).await,
            Reclaimed::Answers { stop }
        );
        assert_eq!(generation.signalled.get(), None);
    }

    let generation = Scripted {
        stop: refusal(STORE_ERROR),
        ..Scripted::silent_holder()
    };
    let rules = ReclaimRules {
        store_error_is_silence: true,
        ..operator()
    };
    assert_eq!(
        reclaim(&generation, None, rules).await,
        Reclaimed::Gone { forced: true }
    );
}

/// A silent daemon is signalled only as the process the lock and the pidfile vouch for.
#[tokio::test]
async fn a_silent_daemon_is_not_signalled_without_the_lock_and_the_pidfile() {
    let lock_free = Scripted {
        lock: LockState::Free,
        ..Scripted::silent_holder()
    };
    assert_eq!(
        reclaim(&lock_free, Some(DAEMON_PID), operator()).await,
        Reclaimed::Unidentified
    );
    assert_eq!(lock_free.signalled.get(), None);

    let unrecorded = Scripted {
        holder: None,
        ..Scripted::silent_holder()
    };
    assert_eq!(reclaim(&unrecorded, None, operator()).await, Reclaimed::Unidentified);
    assert_eq!(unrecorded.signalled.get(), None);

    let another_pid = Scripted {
        holder: Some(DAEMON_PID + 1),
        ..Scripted::silent_holder()
    };
    assert_eq!(
        reclaim(&another_pid, Some(DAEMON_PID), operator()).await,
        Reclaimed::Unidentified,
        "the pidfile must name the pid the daemon reported"
    );
    assert_eq!(another_pid.signalled.get(), None);
}

/// A pidfile outlives a daemon that was killed, and its pid is handed out again. The lock can
/// read held all the same: a new daemon between taking it and writing its record, or another
/// process testing it at that instant. A process younger than the record did not write it, and a
/// lock that reads free once the pidfile is read vouches for nobody.
#[tokio::test]
async fn a_process_given_the_dead_daemons_pid_is_not_signalled_though_the_lock_reads_held() {
    let reused_pid = Scripted {
        holder_predates_record: false,
        ..Scripted::silent_holder()
    };
    for reported in [Some(DAEMON_PID), None] {
        assert_eq!(
            reclaim(&reused_pid, reported, operator()).await,
            Reclaimed::Unidentified
        );
    }
    assert_eq!(
        reused_pid.signalled.get(),
        None,
        "it started after the record was written"
    );

    let released = Scripted {
        lock_after_pidfile: Some(LockState::Free),
        ..Scripted::silent_holder()
    };
    assert_eq!(reclaim(&released, None, operator()).await, Reclaimed::Unidentified);
    assert_eq!(
        released.signalled.get(),
        None,
        "the lock is read again after the pidfile"
    );
}

/// A daemon that left during the exit window is gone, and nothing is asked or signalled.
#[tokio::test]
async fn a_daemon_that_left_by_itself_is_gone_without_a_signal() {
    let generation = Scripted {
        process_live: false,
        lock: LockState::Free,
        accepts: false,
        ..Scripted::silent_holder()
    };
    for reported in [Some(DAEMON_PID), None] {
        assert_eq!(
            reclaim(&generation, reported, operator()).await,
            Reclaimed::Gone { forced: false }
        );
    }
    assert!(generation.asked().is_empty());
    assert_eq!(generation.signalled.get(), None);
}

/// With no answer to `Status` the takeover has nothing to classify and leaves the decision to the
/// caller's ping; it asks nobody to stop.
#[tokio::test]
async fn a_takeover_asks_nothing_of_a_daemon_that_does_not_answer_status() {
    let generation = Scripted {
        version: None,
        ..Scripted::silent_holder()
    };
    assert_eq!(take_over(&generation).await.expect("settled"), Existing::Spawn);
    assert_eq!(generation.asked(), ["status"]);
    assert_eq!(generation.signalled.get(), None);
}
