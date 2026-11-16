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
            "started_unix": now_unix(),
        })
        .to_string();
        std::fs::write(self.generation.join("daemon.pid"), &record).expect("write pidfile");
        let registry = self.data_home.join("daemons").join(format!("g{RENDEZVOUS_GENERATION}"));
        std::fs::create_dir_all(&registry).expect("mkdir registry");
        std::fs::write(registry.join("comms-scripted.pid"), &record).expect("write registry record");
    }

    /// Hold this generation's daemon lock, as the daemon `record_daemon` names does while it runs.
    fn hold_daemon_lock(&self) -> std::fs::File {
        use fs2::FileExt as _;
        let lock = std::fs::File::create(self.generation.join("daemon.lock")).expect("create daemon.lock");
        lock.try_lock_exclusive().expect("hold the daemon lock");
        lock
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
/// script closes the connection without a reply, which is what a wedged daemon looks like. Once
/// it has accepted a `Stop` it closes its listener, as a daemon that drains and exits does.
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
                let response = script(&request);
                let leaving = request == CommsRequest::Stop && response == Some(CommsResponse::Ok);
                if let Some(response) = response {
                    let reply = rmp_serde::to_vec_named(&CommsOut::Response(response)).expect("encode");
                    let len = u32::try_from(reply.len()).expect("len fits");
                    let _ = stream.write_all(&len.to_be_bytes());
                    let _ = stream.write_all(&reply);
                }
                if leaving {
                    break;
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
    status_of(pid, OLDER)
}

/// The `Status` of a daemon of release `version` on a protocol this binary does not speak.
fn status_of(pid: u32, version: &str) -> CommsResponse {
    CommsResponse::Status(StatusReport {
        pid,
        version: version.to_string(),
        build_id: "scripted".to_string(),
        proto_ver: OTHER_PROTO,
        uptime_secs: 1,
        threads: 0,
        subscribers: 0,
    })
}

/// A `basemind serve` child and the host's ends of its pipes. Its stdout is read on a thread, so
/// a relay that never answers fails the test instead of hanging it.
struct Serve {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
    /// Where the child's stderr goes, to explain a failure.
    log: PathBuf,
}

impl Serve {
    fn start(machine: &Machine) -> Self {
        use std::io::BufRead as _;
        use std::process::Stdio;

        let root = machine._tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("mkdir workspace");
        // The config file is what marks a directory as a workspace root `serve` accepts.
        std::fs::write(root.join("basemind.toml"), "\"$schema\" = \"v1\"\n").expect("write basemind.toml");
        let log = machine._tmp.path().join("serve.stderr");
        let stderr = std::fs::File::create(&log).expect("create stderr log");
        let mut child = Command::new(BIN)
            .arg("--root")
            .arg(&root)
            .arg("serve")
            .env("BASEMIND_COMMS_DIR", &machine.comms_dir)
            .env("BASEMIND_DATA_HOME", &machine.data_home)
            .env_remove(NO_AUTOSPAWN_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("spawn basemind serve");
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            log,
        }
    }

    /// Send `initialize` as a host does: once, with id 1.
    fn initialize(&mut self) {
        self.send(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        );
    }

    /// Write one line to the relay, as the host does.
    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to basemind serve");
        self.stdin.flush().expect("flush");
    }

    /// The next line the relay writes to the host.
    fn answer(&self, within: Duration) -> serde_json::Value {
        let line = self.lines.recv_timeout(within).unwrap_or_else(|error| {
            let stderr = std::fs::read_to_string(&self.log).unwrap_or_default();
            panic!("basemind serve must answer the host ({error}); its stderr:\n{stderr}")
        });
        serde_json::from_str(&line).expect("the relay writes JSON lines")
    }

    /// The exit status once the process ends by itself, the host's pipe still open; `None` if it
    /// is still running after `limit`.
    fn exits_within(&mut self, limit: Duration) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll basemind serve") {
                return Some(status);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The main upgrade path through the binary: a session of an older install starts while a newer
/// daemon it cannot talk to holds the generation. The host's `initialize` must be read and
/// answered with the reason, and the process must then end without the host closing its pipe.
#[test]
fn serve_started_under_a_newer_daemon_answers_initialize_and_exits_by_itself() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let pid = stand_in.0;
    let daemon = Scripted::serve(&machine.socket(), move |request| match request {
        CommsRequest::Status => Some(status_of(pid, "999.0.0")),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let mut serve = Serve::start(&machine);
    serve.initialize();

    let answer = serve.answer(Duration::from_secs(60));
    assert_eq!(answer["id"], 1, "{answer}");
    let message = answer["error"]["message"].as_str().expect("an error message");
    assert!(message.contains("update this basemind"), "{answer}");
    assert_eq!(answer["error"]["data"]["retryable"], false, "{answer}");
    let status = serve
        .exits_within(Duration::from_secs(30))
        .expect("a session that can never start ends, with the host's pipe still open");
    assert!(!status.success(), "and reports failure");
    assert_eq!(daemon.asked(), ["status"], "a newer daemon is not asked to stop");
    assert!(stand_in.alive(), "nor signalled");
}

/// The same start under an older daemon that is busy: nothing can be done yet, and something can
/// later. After the hold window the session comes up from this binary, a tool call says why it
/// cannot be served, and the relay stays up to retry.
#[test]
fn serve_started_under_an_older_daemon_that_will_not_stop_says_so_and_keeps_running() {
    /// The relay's hold window, less the slack of timing it from outside the process.
    const HELD_AT_LEAST: Duration = Duration::from_secs(8);
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let pid = stand_in.0;
    let daemon = Scripted::serve(&machine.socket(), move |request| match request {
        CommsRequest::Status => Some(status(pid)),
        CommsRequest::Stop => Some(busy()),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let mut serve = Serve::start(&machine);
    let started = std::time::Instant::now();
    serve.initialize();

    let initialized = serve.answer(Duration::from_secs(90));
    assert_eq!(initialized["id"], 1, "{initialized}");
    assert!(
        initialized["result"]["capabilities"]["tools"].is_object(),
        "initialize is answered, not refused: {initialized}"
    );
    assert!(
        started.elapsed() >= HELD_AT_LEAST,
        "initialize is held for the daemon to become reachable before this binary answers it: {:?}",
        started.elapsed()
    );
    serve.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"index_status","arguments":{}}}"#);
    let answer = serve.answer(Duration::from_secs(10));
    assert_eq!(answer["id"], 2, "{answer}");
    let message = answer["error"]["message"].as_str().expect("an error message");
    assert!(
        message.contains("is still running and did not stop") && message.contains("basemind comms stop"),
        "the reason of the latest attempt, with the way out: {answer}"
    );
    assert_eq!(answer["error"]["data"]["retryable"], true, "{answer}");
    assert!(
        serve.exits_within(Duration::from_secs(1)).is_none(),
        "the relay keeps running and retrying"
    );
    let stops = daemon.asked().iter().filter(|asked| **asked == "stop").count();
    assert_eq!(stops, 1, "the daemon is asked to stop once, not on every retry");
    assert!(stand_in.alive());
}

/// The time a daemon recording itself now would write: a record older than the process under its
/// pid is not taken for that process's.
fn now_unix() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970");
    i64::try_from(now.as_secs()).expect("seconds fit")
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
    let _lock = machine.hold_daemon_lock();
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

/// A process answered the handshake, then went silent, and the pidfile names nobody alive. The
/// command can neither call that "no daemon" nor signal on the registry's word.
#[test]
fn comms_stop_fails_with_what_to_do_when_a_silent_daemon_cannot_be_identified() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    let holder = StandIn::spawn();
    machine.register_only(stand_in.0, holder.0);
    holder.kill();
    let _daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        _ => None,
    });

    let out = machine.run(&["--json", "comms", "stop"], &[]);

    assert!(
        !out.status.success(),
        "something is still listening, so this is not success: {}",
        text(&out.stdout)
    );
    let stderr = text(&out.stderr);
    assert!(stderr.contains("does not name a live comms daemon"), "{stderr}");
    assert!(stderr.contains("lsof"), "it says how to find the process: {stderr}");
    assert!(
        stand_in.alive(),
        "a registry record alone does not license a signal when the pidfile names another process"
    );
}

/// The pidfile is what identifies the process to signal, and it is read when the signal is due,
/// not when the command starts: the exit window is long enough for the holder to change.
#[test]
fn comms_stop_signals_the_process_the_pidfile_names_when_the_exit_window_ends() {
    /// Well inside the stop command's exit window, well after anything it reads at the start.
    const LATE_IN_THE_WINDOW: Duration = Duration::from_secs(7);
    let machine = Machine::new();
    let at_start = StandIn::spawn();
    let at_the_end = StandIn::spawn();
    machine.record_daemon(at_start.0);
    let _lock = machine.hold_daemon_lock();
    let _daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Hello { .. } => Some(proto_skew()),
        _ => None,
    });
    let (pidfile, successor) = (machine.generation.join("daemon.pid"), at_the_end.0);
    let named_at_start = at_start.0;
    let handover = std::thread::spawn(move || {
        std::thread::sleep(LATE_IN_THE_WINDOW);
        let named = std::fs::read_to_string(&pidfile)
            .expect("read pidfile")
            .replace(&format!("\"pid\":{named_at_start}"), &format!("\"pid\":{successor}"));
        std::fs::write(&pidfile, named).expect("rewrite pidfile");
    });

    let out = machine.run(&["--json", "comms", "stop"], &[]);

    handover.join().expect("handover thread");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stop --json prints JSON");
    assert_eq!(report["force_terminated"], 1, "{report}");
    assert!(
        at_start.alive(),
        "the process the pidfile named when the command started is not the holder any more"
    );
    assert!(!at_the_end.alive(), "the process it names at the end is");
}

/// `stop --all` must give a daemon the stop command's patience. One that takes two seconds to
/// answer is serving, not unreachable, and must not be killed by the pid in its registry record.
#[test]
fn comms_stop_all_waits_for_a_slow_daemon_instead_of_killing_it_by_its_registry_pid() {
    const SLOWER_THAN_A_PROBE: Duration = Duration::from_secs(2);
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Stop => {
            std::thread::sleep(SLOWER_THAN_A_PROBE);
            Some(CommsResponse::Ok)
        }
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let out = machine.run(&["--json", "comms", "stop", "--all"], &[]);

    assert!(out.status.success(), "{}", text(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stop --all --json prints JSON");
    assert_eq!(report["stopped"], 1, "{report}");
    assert_eq!(report["force_terminated"], 0, "{report}");
    assert_eq!(daemon.asked(), ["stop"], "its own answer is what stopped it");
    assert!(stand_in.alive(), "a daemon that answered must not be signalled");
}

#[test]
fn comms_stop_all_does_not_signal_a_daemon_that_ignores_stop_but_answers_ping() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let _daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let out = machine.run(&["--json", "comms", "stop", "--all"], &[]);

    assert!(!out.status.success(), "the daemon was not stopped");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stop --all --json prints JSON");
    assert_eq!(report["force_terminated"], 0, "{report}");
    assert_eq!(report["left_running"][0]["pid"], stand_in.0, "{report}");
    assert!(stand_in.alive(), "a daemon that still answers must not be signalled");
}

/// The same refusal that makes a single `comms stop` fail must make `stop --all` fail: a script
/// that goes on as if every daemon were gone would otherwise find one still serving.
#[test]
fn comms_stop_all_fails_and_names_a_daemon_that_refused_to_stop() {
    let machine = Machine::new();
    let stand_in = StandIn::spawn();
    machine.record_daemon(stand_in.0);
    let daemon = Scripted::serve(&machine.socket(), |request| match request {
        CommsRequest::Stop => Some(busy()),
        CommsRequest::Ping => Some(CommsResponse::Pong),
        _ => None,
    });

    let out = machine.run(&["--json", "comms", "stop", "--all"], &[]);

    assert!(
        !out.status.success(),
        "a daemon is still running: {}",
        text(&out.stdout)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stop --all --json prints JSON");
    assert_eq!(report["stopped"], 0, "{report}");
    assert_eq!(report["refused"][0]["pid"], stand_in.0, "{report}");
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!("pid={} refused [daemon_busy]", stand_in.0)),
        "the error names the daemon and its reason: {stderr}"
    );
    assert_eq!(daemon.asked(), ["stop"], "a refusal is not asked about twice");
    assert!(stand_in.alive(), "a daemon that refused is not signalled");
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

/// Hold `root`'s index lock from this process, as a basemind that is not this build's daemon does:
/// the lock file locked, and the sidecar naming the holder. The registry record is the one a
/// release up to 0.33 writes for its daemon, so the holder can be named by version.
fn hold_index_lock(machine: &Machine, root: &Path, version: &str) -> std::fs::File {
    use basemind::store::{CACHE_DIR, LOCK_FILE, LOCK_META_FILE, workspace_key};
    use fs2::FileExt as _;

    let dir = machine
        .data_home
        .join(CACHE_DIR)
        .join("workspaces")
        .join(workspace_key(root));
    std::fs::create_dir_all(&dir).expect("mkdir workspace cache");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .expect("open the index lock");
    lock.try_lock_exclusive().expect("take the index lock");
    let pid = std::process::id();
    let meta = serde_json::json!({ "command": "basemind rescan", "pid": pid, "acquired_unix": 0 });
    std::fs::write(dir.join(LOCK_META_FILE), meta.to_string()).expect("write the lock sidecar");
    let record = serde_json::json!({
        "pid": pid, "kind": "comms", "dir": machine.comms_dir, "version": version, "started_unix": now_unix(),
    });
    let registry = machine.data_home.join("daemons");
    std::fs::create_dir_all(&registry).expect("mkdir registry");
    std::fs::write(registry.join("comms-other-version.pid"), record.to_string()).expect("write registry record");
    lock
}

/// The upgrade case: a session of another basemind version is open in the repository, so that
/// version's daemon holds the index lock when this build's session starts. The host sends
/// `initialize` once and sees a server that came up: its tools are listed, and a tool call says
/// who holds the index and what to do. Once the holder lets go the same `basemind serve` process
/// connects, tells the host the tool list changed, and serves the calls.
#[test]
fn serve_started_while_another_version_holds_the_index_is_usable_and_connects_when_it_is_released() {
    const HELD_AT_LEAST: Duration = Duration::from_secs(8);
    let machine = Machine::new();
    let root = machine._tmp.path().join("workspace");
    std::fs::create_dir_all(&root).expect("mkdir workspace");
    let held = hold_index_lock(&machine, &root, "0.33.0");

    let mut serve = Serve::start(&machine);
    let started = std::time::Instant::now();
    serve.initialize();
    let initialized = serve.answer(Duration::from_secs(120));
    assert_eq!(initialized["id"], 1, "{initialized}");
    assert_eq!(
        initialized["result"]["capabilities"]["tools"]["listChanged"], true,
        "initialize is answered, with a tool list that can change: {initialized}"
    );
    assert!(
        started.elapsed() >= HELD_AT_LEAST,
        "initialize is held for the index to come free before this binary answers it: {:?}",
        started.elapsed()
    );
    serve.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    serve.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let listed = serve.answer(Duration::from_secs(10));
    assert_eq!(listed["id"], 2, "{listed}");
    assert!(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "index_status")),
        "the tools are listed while the index is held: {listed}"
    );

    let call = |id: u64| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"index_status","arguments":{{}}}}}}"#
        )
    };
    serve.send(&call(3));
    let refused = serve.answer(Duration::from_secs(10));
    assert_eq!(refused["id"], 3, "{refused}");
    let message = refused["error"]["message"].as_str().expect("an error message");
    let holder = format!("the basemind v0.33.0 daemon, pid {}", std::process::id());
    assert!(
        message.contains("another basemind holds this workspace's index") && message.contains(&holder),
        "a tool call names the holder: {refused}"
    );
    assert_eq!(refused["error"]["data"]["retryable"], true, "{refused}");
    assert!(
        serve.exits_within(Duration::from_secs(1)).is_none(),
        "a locked workspace does not end the session"
    );

    drop(held);
    let changed = serve.answer(Duration::from_secs(180));
    assert_eq!(
        changed,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
        "once the index is free the relay connects and says so; initialize is not answered again"
    );
    serve.send(&call(4));
    let served = serve.answer(Duration::from_secs(60));
    assert_eq!(served["id"], 4, "{served}");
    assert!(
        served.get("result").is_some(),
        "the same process serves the session from the daemon: {served}"
    );

    drop(serve);
    machine.run(&["comms", "stop"], &[]);
}
