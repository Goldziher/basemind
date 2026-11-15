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

    fn alive(&self) -> bool {
        pid_is_live(self.0)
    }
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

/// A generation dir whose `daemon.pid` names `recorded_pid`, as the lock holder would have written.
fn generation(dir: &Path, recorded_pid: Option<u32>) -> CommsPaths {
    if let Some(pid) = recorded_pid {
        let record = DaemonRecord {
            pid,
            kind: DaemonKind::Comms,
            dir: dir.to_path_buf(),
            version: OLDER.to_string(),
            started_unix: 0,
        };
        std::fs::write(
            dir.join("daemon.pid"),
            serde_json::to_vec(&record).expect("encode record"),
        )
        .expect("write pidfile");
    }
    CommsPaths {
        comms_dir: dir.to_path_buf(),
        socket_path: dir.join("comms.sock"),
    }
}

async fn settle(paths: &CommsPaths) -> Result<Existing, SingletonError> {
    settle_existing(paths, SpawnPolicy::Explicit, OURS, FAST).await
}

fn report(version: &str, proto_ver: u32) -> StatusReport {
    match status(1, version, proto_ver) {
        CommsResponse::Status(report) => report,
        other => unreachable!("status() builds a Status: {other:?}"),
    }
}

#[test]
fn the_table_never_replaces_a_newer_daemon_and_uses_any_it_can_talk_to() {
    assert_eq!(classify(&report(NEWER, PROTO_VER), OURS), Verdict::Use);
    assert_eq!(classify(&report(NEWER, OTHER_PROTO), OURS), Verdict::RefuseNewer);
    assert_eq!(classify(&report(OURS.version, PROTO_VER), OURS), Verdict::Use);
    assert_eq!(
        classify(&report(OURS.version, OTHER_PROTO), OURS),
        Verdict::Replace { usable: false }
    );
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
    let pid = stand_in.0;
    let _daemon = Scripted::serve(&paths.socket_path, move |request| match request {
        CommsRequest::Status => Some(status(pid, OLDER, OTHER_PROTO)),
        _ => None,
    });

    assert_eq!(settle(&paths).await.expect("reclaimed"), Existing::Spawn);
    assert!(!stand_in.alive(), "the recorded, silent daemon is terminated");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_daemon_is_not_signalled_on_the_strength_of_its_reported_pid_alone() {
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

        assert_eq!(settle(&paths).await.expect("nothing to reclaim"), Existing::Spawn);
        assert!(
            stand_in.alive(),
            "pidfile {recorded:?}: an unrecorded pid must not be signalled"
        );
        assert!(bystander.alive(), "the recorded pid is not the one that was reported");
    }
}
