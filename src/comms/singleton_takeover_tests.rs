//! The takeover table against a scripted daemon and a stand-in process.
//!
//! Each case binds its own socket in its own tempdir and names a real, disposable process as the
//! "daemon" pid, so "was it signalled" is answered by whether that process is still alive.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::comms::protocol::{CommsOut, CommsRequest, CommsResponse, PROTO_VER, StatusReport};
use crate::daemon_lock::{DaemonRecord, pid_is_live};

const OURS: Ours = Ours {
    version: "0.50.0",
    proto_ver: PROTO_VER,
};
const OTHER_PROTO: u32 = PROTO_VER + 1;
const NEWER: &str = "0.51.0";
const OLDER: &str = "0.49.0";

const FAST: Timings = Timings {
    drain: Duration::from_millis(300),
    exit: Duration::from_millis(200),
    term_grace: Duration::from_secs(2),
};

/// A process to stand in for the daemon. Started through a shell that exits at once, so it is
/// reparented to init: a child of this test would linger as a zombie after a signal and still read
/// as live.
struct StandIn(u32);

impl StandIn {
    fn spawn() -> Self {
        let out = std::process::Command::new("sh")
            .args(["-c", "sleep 120 >/dev/null 2>&1 & echo $!"])
            .output()
            .expect("spawn stand-in");
        let pid = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("stand-in pid");
        assert!(pid_is_live(pid), "the stand-in must be running before the test starts");
        Self(pid)
    }

    /// A stand-in that writes `signalled` when it receives SIGTERM and exits by itself, unsignalled,
    /// once `release` exists: a daemon finishing its drain.
    fn spawn_leaving_when(release: &Path, signalled: &Path) -> Self {
        let script = r#"(trap 'echo term > "$2"; exit 0' TERM; while [ ! -e "$1" ]; do sleep 0.05; done) >/dev/null 2>&1 &
echo $!"#;
        let out = std::process::Command::new("sh")
            .args(["-c", script, "sh"])
            .arg(release)
            .arg(signalled)
            .output()
            .expect("spawn stand-in");
        let pid = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("stand-in pid");
        assert!(pid_is_live(pid), "the stand-in must be running before the test starts");
        Self(pid)
    }

    fn alive(&self) -> bool {
        pid_is_live(self.0)
    }
}

/// Create `path` after `delay`, from another thread.
fn create_after(path: &Path, delay: Duration) -> std::thread::JoinHandle<()> {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        std::fs::write(path, b"").expect("create file");
    })
}

impl Drop for StandIn {
    fn drop(&mut self) {
        // SAFETY: the pid is a `sleep` this test started; SIGKILL on an already-dead pid is a no-op.
        unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
    }
}

/// A daemon that answers each request from a script and records what it was asked. `None` from the
/// script closes the connection without a reply, which is what a wedged daemon looks like.
struct Scripted {
    seen: Arc<Mutex<Vec<&'static str>>>,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Scripted {
    fn serve(socket: &Path, script: impl Fn(&CommsRequest) -> Option<CommsResponse> + Send + 'static) -> Self {
        let listener = UnixListener::bind(socket).expect("bind scripted daemon");
        listener.set_nonblocking(true).expect("nonblocking");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (log, stop) = (seen.clone(), done.clone());
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).expect("blocking");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut prefix = [0u8; 4];
                if stream.read_exact(&mut prefix).is_err() {
                    continue;
                }
                let mut body = vec![0u8; u32::from_be_bytes(prefix) as usize];
                if stream.read_exact(&mut body).is_err() {
                    continue;
                }
                let Ok(request) = rmp_serde::from_slice::<CommsRequest>(&body) else {
                    continue;
                };
                log.lock().expect("log").push(match request {
                    CommsRequest::Status => "status",
                    CommsRequest::Stop => "stop",
                    CommsRequest::Ping => "ping",
                    _ => "other",
                });
                if let Some(response) = script(&request) {
                    let reply = rmp_serde::to_vec_named(&CommsOut::Response(response)).expect("encode");
                    let len = u32::try_from(reply.len()).expect("len fits");
                    let _ = stream.write_all(&len.to_be_bytes());
                    let _ = stream.write_all(&reply);
                }
            }
        });
        Self {
            seen,
            done,
            thread: Some(thread),
        }
    }

    /// Everything asked so far, minus liveness pings (their count depends on timing).
    fn asked(&self) -> Vec<&'static str> {
        let seen = self.seen.lock().expect("log");
        seen.iter().copied().filter(|name| *name != "ping").collect()
    }
}

impl Drop for Scripted {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn status(pid: u32, version: &str, proto_ver: u32) -> CommsResponse {
    CommsResponse::Status(StatusReport {
        pid,
        version: version.to_string(),
        build_id: "scripted".to_string(),
        proto_ver,
        uptime_secs: 1,
        threads: 0,
        subscribers: 0,
    })
}

fn busy() -> CommsResponse {
    CommsResponse::Error {
        code: "daemon_busy".to_string(),
        message: "refusing to stop while persistent clients or work are active".to_string(),
    }
}

fn store_error() -> CommsResponse {
    CommsResponse::Error {
        code: "store_error".to_string(),
        message: "fjall error: Poisoned".to_string(),
    }
}

/// Write `dir`'s `daemon.pid` naming `pid`, as the lock holder would have just now.
fn record_holder(dir: &Path, pid: u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970");
    record_holder_since(dir, pid, i64::try_from(now.as_secs()).expect("seconds fit"));
}

/// Write `dir`'s `daemon.pid` naming `pid`, as a daemon that took the lock at `started_unix`.
fn record_holder_since(dir: &Path, pid: u32, started_unix: i64) {
    let record = DaemonRecord {
        pid,
        kind: DaemonKind::Comms,
        dir: dir.to_path_buf(),
        version: OLDER.to_string(),
        started_unix,
        process_start: None,
        boot: None,
    };
    std::fs::write(
        dir.join("daemon.pid"),
        serde_json::to_vec(&record).expect("encode record"),
    )
    .expect("write pidfile");
}

/// A generation dir whose `daemon.pid` names `recorded_pid`, as the lock holder would have written.
fn generation(dir: &Path, recorded_pid: Option<u32>) -> CommsPaths {
    if let Some(pid) = recorded_pid {
        record_holder(dir, pid);
    }
    CommsPaths {
        comms_dir: dir.to_path_buf(),
        socket_path: dir.join("comms.sock"),
    }
}

async fn settle(paths: &CommsPaths) -> Result<Existing, SingletonError> {
    settle_existing(paths, SpawnPolicy::Explicit, Takeover::Ask, OURS, FAST, &mut false).await
}

/// An exit window long enough that a stand-in leaving by itself is seen to leave within it.
const PATIENT: Timings = Timings {
    drain: Duration::from_millis(300),
    exit: Duration::from_secs(20),
    term_grace: Duration::from_secs(2),
};
/// How long after the test starts the draining stand-in leaves: well past the moment a client that
/// did not wait would have signalled it.
const LEAVES_AFTER: Duration = Duration::from_millis(1500);

fn report(version: &str, proto_ver: u32) -> StatusReport {
    match status(1, version, proto_ver) {
        CommsResponse::Status(report) => report,
        other => unreachable!("status() builds a Status: {other:?}"),
    }
}

#[test]
fn the_table_asks_only_an_older_daemon_to_stop() {
    assert_eq!(classify(&report(NEWER, PROTO_VER), OURS), Verdict::Use);
    assert_eq!(classify(&report(NEWER, OTHER_PROTO), OURS), Verdict::RefuseNewer);
    assert_eq!(classify(&report(OURS.version, PROTO_VER), OURS), Verdict::Use);
    assert_eq!(classify(&report(OURS.version, OTHER_PROTO), OURS), Verdict::Use);
    assert_eq!(
        classify(&report(OLDER, PROTO_VER), OURS),
        Verdict::Replace { usable: true }
    );
    assert_eq!(
        classify(&report(OLDER, OTHER_PROTO), OURS),
        Verdict::Replace { usable: false }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_daemon_is_never_asked_to_stop_or_signalled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, NEWER, OTHER_PROTO)),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => Some(CommsResponse::Ok),
    });

    let error = settle(&paths)
        .await
        .expect_err("a newer, incompatible daemon is an error");

    assert!(
        matches!(error, SingletonError::NewerDaemon { pid: reported, .. } if reported == pid),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("update this basemind"), "{message}");
    assert!(message.contains("basemind comms stop"), "{message}");
    assert_eq!(daemon.asked(), ["status"], "it must not be asked to stop");
    assert!(stand_in.alive(), "a newer daemon must not be signalled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_daemon_on_our_protocol_is_used() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, NEWER, PROTO_VER)),
        _ => Some(CommsResponse::Ok),
    });

    assert_eq!(settle(&paths).await.expect("usable"), Existing::Use);
    assert_eq!(daemon.asked(), ["status"]);
    assert!(stand_in.alive());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_refuses_stop_is_reported_not_signalled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        CommsRequest::Stop => Some(busy()),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let error = settle(&paths)
        .await
        .expect_err("an unusable daemon that will not stop is an error");

    match &error {
        SingletonError::BusyPredecessor {
            pid: reported, code, ..
        } => {
            assert_eq!(*reported, pid);
            assert_eq!(code, "daemon_busy");
        }
        other => panic!("expected BusyPredecessor, got {other}"),
    }
    assert!(error.to_string().contains("left running"), "{error}");
    assert_eq!(daemon.asked(), ["status", "stop"]);
    assert!(
        stand_in.alive(),
        "a daemon that answered with a refusal must not be signalled"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_daemon_on_our_protocol_that_refuses_stop_is_used() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, PROTO_VER)),
        CommsRequest::Stop => Some(busy()),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    assert_eq!(settle(&paths).await.expect("usable"), Existing::Use);
    assert_eq!(daemon.asked(), ["status", "stop"]);
    assert!(stand_in.alive());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_ignores_stop_but_answers_ping_is_not_signalled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let error = settle(&paths).await.expect_err("it never left");

    assert!(
        matches!(error, SingletonError::StalePredecessor { pid: reported, .. } if reported == pid),
        "{error}"
    );
    assert!(stand_in.alive(), "a daemon that still answers must not be signalled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_daemon_is_signalled_when_this_generations_pidfile_names_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let _lock = hold_lock(dir.path());
    let pid = stand_in.0;
    let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        _ => None,
    });

    assert_eq!(settle(&paths).await.expect("reclaimed"), Existing::Spawn);
    assert!(!stand_in.alive(), "the recorded, silent daemon is terminated");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_daemon_the_pidfile_does_not_name_is_reported_not_signalled_or_spawned_over() {
    let unrecorded = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let bystander = StandIn::spawn();
    let cases: [(PathBuf, Option<u32>); 2] = [
        (unrecorded.path().to_path_buf(), None),
        (elsewhere.path().to_path_buf(), Some(bystander.0)),
    ];
    for (dir, recorded) in cases {
        let paths = generation(&dir, recorded);
        let pid = stand_in.0;
        let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
            CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
            _ => None,
        });

        let error = settle(&paths)
            .await
            .expect_err("a live process may still hold the lock");

        assert!(
            matches!(error, SingletonError::StalePredecessor { pid: reported, .. } if reported == pid),
            "pidfile {recorded:?}: {error}"
        );
        assert!(
            stand_in.alive(),
            "pidfile {recorded:?}: an unrecorded pid must not be signalled"
        );
        assert!(bystander.alive(), "the recorded pid is not the one that was reported");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_of_the_same_release_on_another_protocol_is_not_asked_to_stop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OURS.version, OTHER_PROTO)),
        _ => Some(CommsResponse::Ok),
    });

    assert_eq!(settle(&paths).await.expect("left to the handshake"), Existing::Use);
    assert_eq!(daemon.asked(), ["status"], "it must not be asked to stop");
    assert!(stand_in.alive());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_accepts_stop_and_exits_is_not_signalled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (release, signalled) = (dir.path().join("release"), dir.path().join("signalled"));
    let stand_in = StandIn::spawn_leaving_when(&release, &signalled);
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        CommsRequest::Stop => Some(CommsResponse::Ok),
        _ => None,
    });
    let leaving = create_after(&release, LEAVES_AFTER);

    let settled = settle_existing(&paths, SpawnPolicy::Explicit, Takeover::Ask, OURS, PATIENT, &mut false).await;

    leaving.join().expect("release thread");
    assert_eq!(settled.expect("it left"), Existing::Spawn);
    assert_eq!(daemon.asked(), ["status", "stop"]);
    assert!(!stand_in.alive(), "the daemon exited");
    assert!(
        !signalled.exists(),
        "a daemon that exits after accepting Stop is not signalled"
    );
}

/// The second client of a takeover: another client's `Stop` was already accepted (or the daemon is
/// leaving on its idle timer), so this one finds the socket closed and gets no answer at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_draining_for_another_client_gets_the_exit_window_before_any_signal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (release, signalled) = (dir.path().join("release"), dir.path().join("signalled"));
    let stand_in = StandIn::spawn_leaving_when(&release, &signalled);
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        _ => None,
    });
    let leaving = create_after(&release, LEAVES_AFTER);

    let settled = settle_existing(&paths, SpawnPolicy::Explicit, Takeover::Ask, OURS, PATIENT, &mut false).await;

    leaving.join().expect("release thread");
    assert_eq!(settled.expect("it left by itself"), Existing::Spawn);
    assert!(!stand_in.alive(), "the daemon exited");
    assert!(
        !signalled.exists(),
        "a draining daemon must not be signalled inside its exit window"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pidfile_is_read_again_at_the_end_of_the_exit_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let successor = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        _ => None,
    });
    // The handover comes late in the exit window: long after a read made when the window opened,
    // which would still see the first holder and signal it at the end.
    let timings = Timings {
        exit: Duration::from_secs(3),
        ..FAST
    };
    let (comms_dir, successor_pid) = (dir.path().to_path_buf(), successor.0);
    let handover = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2000));
        record_holder(&comms_dir, successor_pid);
    });

    let error = settle_existing(&paths, SpawnPolicy::Explicit, Takeover::Ask, OURS, timings, &mut false)
        .await
        .expect_err("the reported pid no longer holds this generation");

    handover.join().expect("handover thread");
    assert!(matches!(error, SingletonError::StalePredecessor { .. }), "{error}");
    assert!(
        stand_in.alive(),
        "a pid the pidfile stopped naming must not be signalled"
    );
    assert!(successor.alive());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_refusing_with_a_store_error_is_reported_with_its_store_and_the_way_out() {
    for proto_ver in [PROTO_VER, OTHER_PROTO] {
        let dir = tempfile::tempdir().expect("tempdir");
        let stand_in = StandIn::spawn();
        let paths = generation(dir.path(), Some(stand_in.0));
        let pid = stand_in.0;
        let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
            CommsRequest::Status => Some(status(pid, OLDER, proto_ver)),
            CommsRequest::Stop => Some(store_error()),
            CommsRequest::Ping => Some(CommsResponse::Pong),
            _ => None,
        });

        let error = settle(&paths)
            .await
            .expect_err("a daemon with a broken store cannot be used");

        assert!(
            matches!(&error, SingletonError::FailingPredecessor { pid: reported, .. } if *reported == pid),
            "{error}"
        );
        let message = error.to_string();
        let store = dir.path().join("store.fjall");
        assert!(message.contains(&store.display().to_string()), "{message}");
        assert!(message.contains("Poisoned"), "{message}");
        assert!(message.contains("basemind comms stop --all"), "{message}");
        assert!(message.contains("basemind comms doctor --probe"), "{message}");
        assert!(
            !message.contains("idle"),
            "a broken store does not clear when sessions end: {message}"
        );
        assert!(stand_in.alive(), "a daemon that answered must not be signalled");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_that_already_asked_does_not_send_stop_again() {
    for (proto_ver, usable) in [(PROTO_VER, true), (OTHER_PROTO, false)] {
        let dir = tempfile::tempdir().expect("tempdir");
        let stand_in = StandIn::spawn();
        let paths = generation(dir.path(), Some(stand_in.0));
        let pid = stand_in.0;
        let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
            CommsRequest::Status => Some(status(pid, OLDER, proto_ver)),
            CommsRequest::Ping => Some(CommsResponse::Pong),
            _ => Some(CommsResponse::Ok),
        });

        let settled = settle_existing(&paths, SpawnPolicy::Explicit, Takeover::Wait, OURS, FAST, &mut false).await;

        if usable {
            assert_eq!(settled.expect("an older daemon on our protocol is used"), Existing::Use);
        } else {
            let error = settled.expect_err("an older daemon on another protocol is still in the way");
            assert!(matches!(error, SingletonError::StalePredecessor { .. }), "{error}");
        }
        assert_eq!(daemon.asked(), ["status"], "nothing but Status may be sent");
        assert!(stand_in.alive());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_reports_whether_it_sent_stop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    for (version, expect_stop) in [(NEWER, false), (OLDER, true)] {
        let socket = dir.path().join(format!("{version}.sock"));
        let paths = CommsPaths {
            socket_path: socket.clone(),
            ..paths.clone()
        };
        let _daemon = Scripted::serve(&socket, move |request| match request {
            CommsRequest::Status => Some(status(pid, version, PROTO_VER)),
            CommsRequest::Stop => Some(busy()),
            _ => Some(CommsResponse::Pong),
        });
        let mut stop_sent = false;

        let settled = settle_existing(&paths, SpawnPolicy::Explicit, Takeover::Ask, OURS, FAST, &mut stop_sent).await;

        assert_eq!(settled.expect("usable either way"), Existing::Use);
        assert_eq!(stop_sent, expect_stop, "daemon {version}");
    }
    let mut stop_sent = false;
    let nothing = CommsPaths {
        socket_path: dir.path().join("nothing.sock"),
        ..paths
    };
    let settled = settle_existing(
        &nothing,
        SpawnPolicy::Explicit,
        Takeover::Ask,
        OURS,
        FAST,
        &mut stop_sent,
    )
    .await;
    assert_eq!(settled.expect("nothing to settle"), Existing::Spawn);
    assert!(!stop_sent, "with no daemon there is nobody to ask");
}

/// The same window, seen from its other end: a daemon that was silent throughout and answers
/// again when the window closes is alive and serving, whatever its pidfile says.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_silent_through_the_exit_window_that_answers_ping_at_its_end_is_not_signalled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let pid = stand_in.0;
    let awake = Arc::new(AtomicBool::new(false));
    let answering = awake.clone();
    let daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        CommsRequest::Ping if answering.load(Ordering::SeqCst) => Some(CommsResponse::Pong),
        _ => None,
    });
    let timings = Timings {
        exit: Duration::from_secs(2),
        ..FAST
    };
    let waking = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        awake.store(true, Ordering::SeqCst);
    });

    let error = settle_existing(&paths, SpawnPolicy::Explicit, Takeover::Ask, OURS, timings, &mut false)
        .await
        .expect_err("the daemon is answering again");

    waking.join().expect("waking thread");
    assert!(matches!(error, SingletonError::StalePredecessor { .. }), "{error}");
    assert!(
        stand_in.alive(),
        "a daemon that answers at the end of the window must not be signalled"
    );
    assert_eq!(
        daemon.asked(),
        ["status", "stop", "stop"],
        "it is asked once more before a signal is considered"
    );
}

/// The rules `basemind comms stop` reclaims by, with the waits cut short.
const QUICK: ReclaimRules = ReclaimRules {
    exit: Duration::from_millis(200),
    term_grace: Duration::from_secs(2),
    probe_timeout: Duration::from_millis(300),
    store_error_is_silence: false,
};

/// A pid no process has.
const DEAD_PID: u32 = 0x7FFF_FFFE;

/// Hold `dir`'s daemon lock, as a running daemon does.
fn hold_lock(dir: &Path) -> std::fs::File {
    use fs2::FileExt as _;
    let lock = std::fs::File::create(dir.join("daemon.lock")).expect("create daemon.lock");
    lock.try_lock_exclusive().expect("hold the daemon lock");
    lock
}

/// `comms stop` has no pid from the daemon itself. Whether a daemon is still there is then read
/// from the lock and the endpoint, never from the pidfile, which outlives a daemon that crashed.
#[tokio::test(flavor = "multi_thread")]
async fn with_no_reported_pid_a_free_lock_and_a_closed_endpoint_mean_the_daemon_is_gone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bystander = StandIn::spawn();
    let paths = generation(dir.path(), Some(bystander.0));

    assert_eq!(
        reclaim_holder(&paths, None, QUICK).await,
        Reclaimed::Gone { forced: false }
    );
    assert!(
        bystander.alive(),
        "a pidfile left behind names a process that is nobody's daemon now"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_reported_pid_a_silent_daemon_is_terminated_as_the_process_the_pidfile_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let _lock = hold_lock(dir.path());
    let daemon = Scripted::serve(&paths.socket_path, |_| None);

    assert_eq!(
        reclaim_holder(&paths, None, QUICK).await,
        Reclaimed::Gone { forced: true }
    );
    assert!(!stand_in.alive(), "the recorded, silent daemon is terminated");
    assert_eq!(daemon.asked(), ["stop"], "after it was asked once more");
}

/// The daemon died inside the exit window and its pid went to another process: the status reply
/// and the pidfile both still name that number, and it is live. The lock is what the dead daemon
/// could not keep, so a lock that reads free means there is nobody to signal, whoever the caller
/// thinks it is addressing.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_process_under_the_daemons_old_pid_is_not_signalled_once_the_lock_is_free() {
    for reported in [true, false] {
        let dir = tempfile::tempdir().expect("tempdir");
        let reused = StandIn::spawn();
        let paths = generation(dir.path(), Some(reused.0));
        drop(hold_lock(dir.path()));
        let silent = Scripted::serve(&paths.socket_path, |_| None);

        let listening = reclaim_holder(&paths, reported.then_some(reused.0), QUICK).await;
        assert_eq!(
            listening,
            Reclaimed::Unidentified,
            "reported pid {reported}: a socket that still accepts is somebody, but not the lock's holder"
        );
        assert!(reused.alive(), "reported pid {reported}: nothing is signalled");

        drop(silent);
        let gone = reclaim_holder(&paths, reported.then_some(reused.0), QUICK).await;
        assert_eq!(
            gone,
            Reclaimed::Gone { forced: false },
            "reported pid {reported}: a free lock and a closed socket are a daemon that is gone"
        );
        assert!(reused.alive(), "reported pid {reported}: nothing is signalled");
    }
}

/// The pidfile of a daemon killed long ago names a pid that a younger process now has, and the
/// lock reads held (a new daemon that has not written its record yet). The process did not write
/// that pidfile, so it is not signalled.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_live_process_younger_than_the_pidfile_is_not_signalled_though_the_lock_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reused = StandIn::spawn();
    let paths = generation(dir.path(), None);
    record_holder_since(dir.path(), reused.0, 1);
    let _lock = hold_lock(dir.path());
    let _silent = Scripted::serve(&paths.socket_path, |_| None);

    assert_eq!(reclaim_holder(&paths, None, QUICK).await, Reclaimed::Unidentified);
    assert!(
        reused.alive(),
        "a process that started after the record is not its daemon"
    );
}

/// Something is plainly still there — it holds the lock, or its socket still accepts — and the
/// pidfile cannot say which process it is. That is neither "no daemon" nor a licence to signal.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_is_there_but_cannot_be_identified_is_reported_and_nothing_is_signalled() {
    let listening = tempfile::tempdir().expect("tempdir");
    let paths = generation(listening.path(), None);
    let _daemon = Scripted::serve(&paths.socket_path, |_| None);
    assert_eq!(
        reclaim_holder(&paths, None, QUICK).await,
        Reclaimed::Unidentified,
        "no pidfile"
    );

    let locked = tempfile::tempdir().expect("tempdir");
    let paths = generation(locked.path(), Some(DEAD_PID));
    let _lock = hold_lock(locked.path());
    assert_eq!(
        reclaim_holder(&paths, None, QUICK).await,
        Reclaimed::Unidentified,
        "a pidfile naming a process that is gone, under a lock that is still held"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_answers_the_stop_sent_at_the_end_of_the_window_is_left_alone() {
    for answer in [busy(), CommsResponse::Ok] {
        let dir = tempfile::tempdir().expect("tempdir");
        let stand_in = StandIn::spawn();
        let paths = generation(dir.path(), Some(stand_in.0));
        let reply = answer.clone();
        let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
            CommsRequest::Stop => Some(reply.clone()),
            _ => None,
        });

        let reclaimed = reclaim_holder(&paths, None, QUICK).await;

        assert!(
            matches!(&reclaimed, Reclaimed::Answers { stop } if *stop != StopOutcome::Unreachable),
            "{answer:?}: {reclaimed:?}"
        );
        assert!(
            stand_in.alive(),
            "{answer:?}: a daemon that answers must not be signalled"
        );
    }
}

/// A daemon that answers only to say its store is broken is spared by a takeover and reclaimed
/// by an operator's `comms stop`, which exists for exactly that daemon.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_error_spares_the_daemon_unless_the_caller_counts_it_as_silence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stand_in = StandIn::spawn();
    let paths = generation(dir.path(), Some(stand_in.0));
    let _lock = hold_lock(dir.path());
    let _daemon = Scripted::serve(&paths.socket_path, |request| match request {
        CommsRequest::Stop => Some(store_error()),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let spared = reclaim_holder(&paths, None, QUICK).await;
    assert!(matches!(spared, Reclaimed::Answers { .. }), "{spared:?}");
    assert!(stand_in.alive());

    let operator = ReclaimRules {
        store_error_is_silence: true,
        ..QUICK
    };
    assert_eq!(
        reclaim_holder(&paths, None, operator).await,
        Reclaimed::Gone { forced: true }
    );
    assert!(!stand_in.alive());
}
