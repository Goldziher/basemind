//! What the real `basemind` binary does about a daemon of another build in its own generation.
//!
//! The daemon is scripted: a thread answers each request from a closure and records what it was
//! asked, and a disposable process stands in for the daemon's pid, so "was it signalled" is whether
//! that process is still alive. Each case gets its own comms directory and its own data home.

#![cfg(all(feature = "comms", unix))]

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use basemind::comms::protocol::{CommsOut, CommsRequest, CommsResponse, PROTO_VER, StatusReport};
use basemind::comms::singleton::{NO_AUTOSPAWN_ENV, RENDEZVOUS_GENERATION, comms_socket_path, generation_dir};
use basemind::daemon_lock::pid_is_live;

const BIN: &str = env!("CARGO_BIN_EXE_basemind");
/// A release older than any this binary can be, so the takeover table treats the daemon as older.
const OLDER: &str = "0.0.1";
const OTHER_PROTO: u32 = PROTO_VER + 1;

/// A process to stand in for the daemon. Started through a shell that exits at once, so it is
/// reparented to init: a child of this test would linger as a zombie after a signal and still read
/// as live.
struct StandIn(u32);

impl StandIn {
    fn spawn() -> Self {
        let out = Command::new("sh")
            .args(["-c", "sleep 300 >/dev/null 2>&1 & echo $!"])
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

    fn kill(&self) {
        let _ = Command::new("kill").args(["-9", &self.0.to_string()]).output();
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.kill();
    }
}

/// One isolated machine: a comms directory, a separate data home, and this generation's directory
/// inside the comms directory.
struct Machine {
    _tmp: tempfile::TempDir,
    comms_dir: PathBuf,
    data_home: PathBuf,
    generation: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let comms_dir = tmp.path().join("comms");
        let data_home = tmp.path().join("data");
        let generation = generation_dir(&comms_dir);
        std::fs::create_dir_all(&generation).expect("mkdir generation");
        std::fs::create_dir_all(&data_home).expect("mkdir data home");
        Self {
            _tmp: tmp,
            comms_dir,
            data_home,
            generation,
        }
    }

    fn socket(&self) -> PathBuf {
        comms_socket_path(&self.generation)
    }

    /// Record `pid` as this generation's daemon, in its pidfile and in the machine registry, the way
    /// the lock holder does.
    fn record_daemon(&self, pid: u32) {
        let record = serde_json::json!({
            "pid": pid,
            "kind": "comms",
            "dir": self.generation,
            "version": OLDER,
            "started_unix": 0,
        })
        .to_string();
        std::fs::write(self.generation.join("daemon.pid"), &record).expect("write pidfile");
        let registry = self.data_home.join("daemons").join(format!("g{RENDEZVOUS_GENERATION}"));
        std::fs::create_dir_all(&registry).expect("mkdir registry");
        std::fs::write(registry.join("comms-scripted.pid"), &record).expect("write registry record");
    }

    /// Record `pid` in the machine registry only, leaving the pidfile to name someone else.
    fn register_only(&self, pid: u32, pidfile_pid: u32) {
        self.record_daemon(pid);
        let pidfile = self.generation.join("daemon.pid");
        let named = std::fs::read_to_string(&pidfile)
            .expect("read pidfile")
            .replace(&format!("\"pid\":{pid}"), &format!("\"pid\":{pidfile_pid}"));
        std::fs::write(pidfile, named).expect("rewrite pidfile");
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .env("BASEMIND_COMMS_DIR", &self.comms_dir)
            .env("BASEMIND_DATA_HOME", &self.data_home)
            .env_remove(NO_AUTOSPAWN_ENV);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("run basemind")
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
                    CommsRequest::Hello { .. } => "hello",
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

/// How a daemon answers a `Hello` carrying a protocol version that is not its own.
fn proto_skew() -> CommsResponse {
    CommsResponse::Error {
        code: "proto_skew".to_string(),
        message: format!("daemon speaks proto {OTHER_PROTO}, client sent {PROTO_VER}"),
    }
}

fn busy() -> CommsResponse {
    CommsResponse::Error {
        code: "daemon_busy".to_string(),
        message: "refusing to stop while persistent clients or work are active".to_string(),
    }
}

fn status(pid: u32) -> CommsResponse {
    CommsResponse::Status(StatusReport {
        pid,
        version: OLDER.to_string(),
        build_id: "scripted".to_string(),
        proto_ver: OTHER_PROTO,
        uptime_secs: 1,
        threads: 0,
        subscribers: 0,
    })
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn comms_stop_reaches_a_daemon_that_refuses_the_handshake_for_protocol_skew() {
    let machine = Machine::new();
    let daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        CommsRequest::Stop => Some(CommsResponse::Ok),
        _ => None,
    });

    let out = machine.run(&["comms", "stop"], &[]);

    assert!(
        out.status.success(),
        "comms stop must reach a daemon on another protocol: {}",
        text(&out.stderr)
    );
    assert_eq!(daemon.asked(), ["hello", "stop"]);
    assert!(
        text(&out.stdout).contains("comms daemon stopping"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn comms_stop_honours_a_refusal_from_a_daemon_on_another_protocol() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        CommsRequest::Stop => Some(busy()),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let out = machine.run(&["comms", "stop"], &[]);

    assert!(!out.status.success(), "a refusal is reported as a failure");
    assert!(text(&out.stderr).contains("daemon_busy"), "{}", text(&out.stderr));
    assert_eq!(daemon.asked(), ["hello", "stop"]);
    assert!(stand_in.alive(), "a daemon that refused must not be signalled");
}

/// A daemon serving sessions can take longer than a liveness probe allows to answer `Stop`. It must
/// be given the stop command's own timeout, not written off as unreachable and killed.
#[test]
fn comms_stop_waits_for_a_slow_answer_from_a_daemon_on_another_protocol() {
    const SLOWER_THAN_A_PROBE: Duration = Duration::from_secs(2);
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let pid = stand_in.0;
    let _daemon = Scripted::serve(&machine.socket(), move |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        CommsRequest::Stop => {
            std::thread::sleep(SLOWER_THAN_A_PROBE);
            // The daemon drains and exits before it confirms.
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
            Some(CommsResponse::Ok)
        }
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let out = machine.run(&["--json", "comms", "stop"], &[]);

    assert!(out.status.success(), "{}", text(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stop --json prints JSON");
    assert_eq!(
        report,
        serde_json::json!({ "stopped": true, "force_terminated": 0 }),
        "the daemon's own answer is what stopped it"
    );
}

#[test]
fn comms_stop_does_not_kill_a_daemon_that_ignores_stop_but_answers_ping() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let _daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let out = machine.run(&["comms", "stop"], &[]);

    assert!(!out.status.success(), "the daemon was not stopped");
    assert!(
        text(&out.stderr).contains("still answers pings"),
        "{}",
        text(&out.stderr)
    );
    assert!(stand_in.alive(), "a daemon that still answers must not be signalled");
}

#[test]
fn comms_stop_terminates_a_silent_daemon_on_another_protocol_that_the_pidfile_names() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let _daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        _ => None,
    });

    let out = machine.run(&["--json", "comms", "stop"], &[]);

    assert!(out.status.success(), "{}", text(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stop --json prints JSON");
    assert_eq!(report["force_terminated"], 1, "{report}");
    assert!(!stand_in.alive(), "the recorded, silent daemon is terminated");
}

#[test]
fn comms_stop_does_not_terminate_a_silent_process_the_pidfile_does_not_name() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    let holder = StandIn::spawn();
    machine.register_only(stand_in.0, holder.0);
    // The pidfile's holder leaves by itself, so the command has nothing to wait out.
    holder.kill();
    let _daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        _ => None,
    });

    let out = machine.run(&["--json", "comms", "stop"], &[]);

    assert!(out.status.success(), "{}", text(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stop --json prints JSON");
    assert_eq!(report["force_terminated"], 0, "{report}");
    assert!(
        stand_in.alive(),
        "a registry record alone does not license a signal when the pidfile names another process"
    );
}

/// `BASEMIND_NO_AUTOSPAWN` in the client's own environment: an older daemon is used as it is,
/// because the client could not replace it. Without the variable the same client asks it to stop.
#[test]
fn a_client_forbidden_to_spawn_does_not_ask_an_older_daemon_to_stop() {
    let serve = |machine: &Machine, pid: u32| {
        Scripted::serve(&machine.socket(), move |request| match request {
            CommsRequest::Status => Some(status(pid)),
            CommsRequest::Stop => Some(busy()),
            CommsRequest::Ping => Some(CommsResponse::Pong),
            _ => None,
        })
    };

    let connect_only = Machine::new();
    let stand_in = StandIn::spawn();
    connect_only.record_daemon(stand_in.0);
    let daemon = serve(&connect_only, stand_in.0);
    let out = connect_only.run(&["daemon", "ensure"], &[(NO_AUTOSPAWN_ENV, "1")]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(daemon.asked(), ["status"], "connect-only must not ask a daemon to stop");
    assert!(stand_in.alive());

    let free_to_spawn = Machine::new();
    free_to_spawn.record_daemon(stand_in.0);
    let daemon = serve(&free_to_spawn, stand_in.0);
    let out = free_to_spawn.run(&["daemon", "ensure"], &[]);
    assert!(
        !out.status.success(),
        "an older daemon that refuses to stop is reported"
    );
    assert!(text(&out.stderr).contains("refused to stop"), "{}", text(&out.stderr));
    assert_eq!(daemon.asked(), ["status", "stop"]);
    assert!(stand_in.alive());
}

/// `comms cleanup` through the binary: a dry run removes nothing, a real run removes the idle
/// rendezvous of other generations and of the pre-generation layout, a held lock spares one, and
/// this generation's directory is left alone whatever its age.
#[test]
fn comms_cleanup_removes_only_idle_unlocked_rendezvous_of_other_generations() {
    use fs2::FileExt as _;

    let machine = Machine::new();
    let fill = |dir: &Path| {
        std::fs::create_dir_all(dir.join("store.fjall")).expect("mkdir");
        for name in ["daemon.lock", "daemon.pid", "store.fjall/data"] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
    };
    let idle = machine.comms_dir.join(format!("g{}", RENDEZVOUS_GENERATION + 1));
    let busy = machine.comms_dir.join(format!("g{}", RENDEZVOUS_GENERATION + 2));
    for dir in [&idle, &busy, &machine.generation, &machine.comms_dir] {
        fill(dir);
    }
    std::fs::write(machine.comms_dir.join("comms.sock"), b"").expect("write");
    let lock = std::fs::File::options()
        .read(true)
        .write(true)
        .open(busy.join("daemon.lock"))
        .expect("open lock");
    lock.try_lock_exclusive().expect("hold the busy generation's lock");

    let dry = machine.run(&["comms", "cleanup", "--dry-run", "--older-than", "0"], &[]);
    assert!(dry.status.success(), "{}", text(&dry.stderr));
    assert!(text(&dry.stdout).contains("would remove"), "{}", text(&dry.stdout));
    assert!(idle.join("store.fjall/data").exists(), "a dry run removes nothing");

    let young = machine.run(&["comms", "cleanup"], &[]);
    assert!(young.status.success(), "{}", text(&young.stderr));
    assert!(
        idle.exists(),
        "a rendezvous used today is younger than the default 30 days"
    );

    let out = machine.run(&["comms", "cleanup", "--older-than", "0"], &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!idle.exists(), "{}", text(&out.stdout));
    assert!(!machine.comms_dir.join("comms.sock").exists());
    assert!(!machine.comms_dir.join("store.fjall").exists());
    assert!(!machine.comms_dir.join("daemon.lock").exists());
    assert!(
        busy.join("store.fjall/data").exists(),
        "a held lock spares the directory"
    );
    assert!(
        machine.generation.join("store.fjall/data").exists(),
        "this generation is never removed"
    );
    drop(lock);
}
