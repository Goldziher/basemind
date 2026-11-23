//! Unit tests for the singleton path resolution, bind-as-lock and bring-up control flow.

use super::*;

#[test]
fn the_autospawn_opt_out_reads_a_boolean_and_honours_a_mistyped_one() {
    let cases: &[(Option<&str>, bool)] = &[
        (None, false),
        (Some(""), false),
        (Some("0"), false),
        (Some("false"), false),
        (Some("no"), false),
        (Some("OFF"), false),
        (Some("1"), true),
        (Some("true"), true),
        (Some("Yes"), true),
        (Some("on"), true),
        (Some("maybe"), true),
    ];
    for (value, forbids) in cases {
        assert_eq!(forbids_autospawn(value.map(OsStr::new)), *forbids, "{value:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_non_utf8_autospawn_opt_out_forbids_spawning() {
    use std::os::unix::ffi::OsStrExt;

    assert!(forbids_autospawn(Some(OsStr::from_bytes(b"\xff"))));
}

#[test]
fn version_is_older_orders_releases() {
    assert!(version_is_older("0.6.3", "0.10.0"));
    assert!(version_is_older("0.9.0", "0.10.0"));
    assert!(version_is_older("0.10.0", "0.10.1"));
    assert!(!version_is_older("0.10.0", "0.10.0"));
    assert!(!version_is_older("0.11.0", "0.10.0"));
    assert!(!version_is_older("1.0.0", "0.10.0"));
    assert!(version_is_older("0.9.0-rc.2", "0.10.0"));
}

/// A release candidate's daemon must make way for the release it preceded, and the release's
/// daemon must never be stopped by a candidate left installed.
#[test]
fn version_is_older_orders_pre_releases_by_semver_precedence() {
    assert!(
        version_is_older("0.10.0-rc.1", "0.10.0"),
        "a candidate precedes its release"
    );
    assert!(!version_is_older("0.10.0", "0.10.0-rc.1"));
    assert!(version_is_older("0.10.0-rc.1", "0.10.0-rc.2"));
    assert!(!version_is_older("0.10.0-rc.2", "0.10.0-rc.1"));
    assert!(!version_is_older("0.10.0-rc.1", "0.10.0-rc.1"));
    assert!(
        version_is_older("0.10.0-rc.9", "0.10.0-rc.10"),
        "numeric identifiers compare as numbers"
    );
    assert!(version_is_older("0.10.0-alpha.3", "0.10.0-beta.1"));
    assert!(
        version_is_older("0.10.0-1", "0.10.0-alpha"),
        "numeric precedes alphanumeric"
    );
    assert!(
        version_is_older("0.10.0-rc", "0.10.0-rc.1"),
        "more identifiers is later"
    );
    assert!(
        !version_is_older("0.10.1-rc.1", "0.10.0"),
        "the core decides before the pre-release"
    );
    assert!(
        !version_is_older("0.10.0+build.1", "0.10.0+build.2"),
        "build metadata has no precedence"
    );
    assert!(version_is_older("0.10.0-rc.1+build.9", "0.10.0+build.1"));
}

/// A probe that could not be made is not a silent daemon. Nothing is spawned on it: the daemon
/// may be serving, and a second one would only exit on the lock the first holds.
#[tokio::test]
async fn no_daemon_is_spawned_when_the_endpoint_could_not_be_asked() {
    let paths = CommsPaths {
        comms_dir: PathBuf::from("comms"),
        socket_path: PathBuf::from("comms/endpoint"),
    };
    let spawned = std::cell::Cell::new(false);
    let spawn = |_: &CommsPaths| {
        spawned.set(true);
        Ok(())
    };
    let outcome = ensure_daemon_probing(&paths, SpawnPolicy::Explicit, |_| Liveness::Unknown, spawn).await;
    assert!(
        matches!(&outcome, Err(SingletonError::EndpointUnknown { socket }) if *socket == paths.socket_path),
        "{outcome:?}"
    );
    assert!(!spawned.get(), "nothing is spawned on a guess");

    let answering = ensure_daemon_probing(
        &paths,
        SpawnPolicy::Explicit,
        |_| Liveness::Answers,
        |_| unreachable!("a daemon that answers is used"),
    )
    .await;
    assert!(answering.is_ok(), "{answering:?}");
}

/// A spawn the binary guard refused is reported at once and as itself. Nothing is polled for: no
/// daemon is coming, and waiting out the readiness window is the stall this guards against.
#[tokio::test]
async fn a_refused_daemon_binary_fails_at_once_with_the_refusal() {
    let comms = tempfile::tempdir().expect("tempdir");
    let paths = CommsPaths {
        comms_dir: comms.path().to_path_buf(),
        socket_path: comms.path().join("endpoint"),
    };
    let reason = "it is not a `basemind` binary. Set BASEMIND_DAEMON_BINARY to a real executable";
    let probes = std::cell::Cell::new(0_u32);
    let started = std::time::Instant::now();
    let outcome = ensure_daemon_probing(
        &paths,
        SpawnPolicy::Explicit,
        |_| {
            probes.set(probes.get() + 1);
            Liveness::Silent
        },
        |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                spawn::BinaryRefused::new(reason),
            ))
        },
    )
    .await;

    assert!(
        matches!(&outcome, Err(SingletonError::DaemonBinaryRefused(message)) if message == reason),
        "{outcome:?}"
    );
    assert_eq!(
        probes.get(),
        1,
        "the endpoint is probed once and never polled after the refusal"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the refusal waited {:?}, far inside the {SPAWN_READY_TIMEOUT:?} readiness window",
        started.elapsed()
    );
}

/// Any other spawn failure keeps its path and cause: only the binary guard is its own error.
#[tokio::test]
async fn an_ordinary_spawn_failure_stays_an_io_error() {
    let comms = tempfile::tempdir().expect("tempdir");
    let paths = CommsPaths {
        comms_dir: comms.path().to_path_buf(),
        socket_path: comms.path().join("endpoint"),
    };
    let outcome = ensure_daemon_probing(
        &paths,
        SpawnPolicy::Explicit,
        |_| Liveness::Silent,
        |_| Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "exec failed")),
    )
    .await;

    assert!(
        matches!(&outcome, Err(SingletonError::Io { path, .. }) if *path == paths.socket_path),
        "{outcome:?}"
    );
}

/// Runs on every platform: the name is a pure function of the user and the directory path.
#[test]
fn the_pipe_name_is_stable_and_distinct_per_generation_directory() {
    /// The longest account name Windows allows in `USERNAME`.
    const LONGEST_USERNAME: usize = 20;
    /// `CreateNamedPipe` rejects a name longer than this, prefix included.
    const PIPE_NAME_MAX: usize = 256;

    assert_eq!(
        pipe_name("alice", "pinned-dir"),
        r"\\.\pipe\basemind-comms-alice-945729672699ae08",
        "every binary of a generation must compute the same name for a directory"
    );

    let dir = |base: &str| generation_dir(Path::new(base)).to_string_lossy().into_owned();
    let name = pipe_name("alice", &dir("comms"));
    assert_eq!(name, pipe_name("alice", &dir("comms")));
    assert_ne!(name, pipe_name("alice", "comms"), "not the unscoped directory's pipe");
    assert_ne!(name, pipe_name("alice", &dir("other")));
    assert_ne!(name, pipe_name("bob", &dir("comms")));

    let deep = dir(&format!("x/{}", "y".repeat(4096)));
    let longest = pipe_name(&"u".repeat(LONGEST_USERNAME), &deep);
    assert!(longest.len() <= PIPE_NAME_MAX, "{} bytes: {longest}", longest.len());
}

/// One directory has one `daemon.lock`, so however its path is spelt it must have one pipe: two
/// names would let two clients each see no daemon and the second daemon exit on the lock.
#[test]
fn every_spelling_of_one_directory_gives_one_pipe_name() {
    let name = pipe_name("alice", r"C:\x\comms\g1");
    for spelling in [
        r"C:\x\comms\g1\",
        "C:/x/comms/g1",
        "C:/x/comms/g1/",
        r"C:\x\\comms\.\g1",
        r"C:\x/comms\g1",
        r"\\?\C:\x\comms\g1",
    ] {
        assert_eq!(pipe_name("alice", spelling), name, "{spelling}");
    }
    assert_eq!(
        pipe_name("alice", r"\\?\UNC\server\share\comms"),
        pipe_name("alice", r"\\server\share\comms"),
        "a canonicalised network path is the same directory"
    );

    assert_ne!(pipe_name("alice", r"C:\x\comms\g2"), name, "another generation");
    assert_ne!(
        pipe_name("alice", r"C:\x\commsg1"),
        name,
        "components, not their concatenation"
    );
    assert_ne!(pipe_name("alice", r"C:\xcomms\g1"), name);
    assert_ne!(pipe_name("alice", r"D:\x\comms\g1"), name);
}

/// `USERNAME` is free text. A backslash in it would end the pipe's name early, and two accounts
/// must not share a pipe because their names look alike once made safe.
#[test]
fn the_account_name_is_made_safe_for_a_pipe_name_and_still_tells_accounts_apart() {
    const PREFIX: &str = r"\\.\pipe\";
    let odd = pipe_name(r"CORP\al ice", "comms");
    let leaf = odd.strip_prefix(PREFIX).expect("a local pipe");
    assert!(
        leaf.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "{odd}"
    );
    assert!(leaf.starts_with("basemind-comms-CORP_al_ice-"), "{odd}");
    assert_ne!(
        odd,
        pipe_name("CORP_al_ice", "comms"),
        "the hash covers the name as given"
    );

    let long = pipe_name(&"é".repeat(300), "comms");
    assert!(long.len() < 100, "{} bytes: {long}", long.len());
}

#[cfg(windows)]
#[test]
fn the_windows_endpoint_is_the_pipe_named_for_the_generation_directory() {
    let one = generation_dir(Path::new(r"C:\one\comms"));
    let two = generation_dir(Path::new(r"C:\two\comms"));
    assert_eq!(comms_socket_path(&one), comms_socket_path(&one));
    assert_eq!(
        comms_socket_path(&one),
        comms_socket_path(Path::new(&format!("C:/one/comms/g{RENDEZVOUS_GENERATION}/"))),
        "the same directory, spelt another way"
    );
    assert_ne!(comms_socket_path(&one), comms_socket_path(&two));
    assert_ne!(comms_socket_path(&one), comms_socket_path(Path::new(r"C:\one\comms")));

    let existing = tempfile::tempdir().expect("tempdir");
    let spelt = PathBuf::from(format!("{}\\.\\", existing.path().display()));
    assert_eq!(
        comms_socket_path(existing.path()),
        comms_socket_path(&spelt),
        "a directory that exists is resolved before it is named"
    );
}

#[cfg(unix)]
#[test]
fn bind_as_lock_admits_exactly_one_winner_in_a_race() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("race.sock");
    let winners = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    const N: usize = 16;
    let listeners = Arc::new(std::sync::Mutex::new(Vec::new()));

    for _ in 0..N {
        let socket = socket.clone();
        let winners = winners.clone();
        let listeners = listeners.clone();
        handles.push(std::thread::spawn(move || {
            let probe = |p: &std::path::Path| p.exists();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("rt");
            let result = rt.block_on(async { bind_listener(&socket, probe) });
            if let Ok(listener) = result {
                winners.fetch_add(1, Ordering::SeqCst);
                listeners.lock().expect("lock").push((listener, rt));
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    assert_eq!(
        winners.load(Ordering::SeqCst),
        1,
        "exactly one binder may win the singleton lock"
    );
}

#[cfg(unix)]
#[test]
fn daemon_status_decodes_the_commsout_response_envelope() {
    use super::super::protocol::{CommsOut, CommsResponse, StatusReport};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("status.sock");
    let listener = UnixListener::bind(&socket).expect("bind");

    let want = StatusReport {
        pid: 4242,
        version: "0.22.4".to_string(),
        build_id: "deadbeef1234".to_string(),
        proto_ver: PROTO_VER,
        uptime_secs: 99,
        agent_store: "open".to_string(),
    };
    let reply = want.clone();
    // A faithful daemon: read the framed request, reply with the SAME CommsOut::Response ~keep
    // envelope the real UDS front-end sends (`frontend_uds::send`), not a bare CommsResponse. ~keep
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).expect("read len");
        let len = u32::from_be_bytes(prefix) as usize;
        let mut req = vec![0u8; len];
        stream.read_exact(&mut req).expect("read req");
        let body = rmp_serde::to_vec_named(&CommsOut::Response(CommsResponse::Status(reply))).expect("encode");
        let out_len = u32::try_from(body.len()).expect("len fits");
        stream.write_all(&out_len.to_be_bytes()).expect("write len");
        stream.write_all(&body).expect("write body");
    });

    let got = daemon_status(&socket).expect("daemon_status must decode the CommsOut envelope");
    server.join().expect("server thread");
    assert_eq!(got.pid, want.pid);
    assert_eq!(got.version, want.version);
    assert_eq!(got.proto_ver, want.proto_ver);
}

#[tokio::test]
async fn ensure_daemon_noops_when_already_alive() {
    let paths = CommsPaths {
        comms_dir: PathBuf::from("/tmp/x"),
        socket_path: PathBuf::from("/tmp/x/comms.sock"),
    };
    let spawned = std::cell::Cell::new(false);
    let res = ensure_daemon_with(
        &paths,
        |_| true,
        |_| {
            spawned.set(true);
            Ok(())
        },
    )
    .await;
    assert!(res.is_ok());
    assert!(!spawned.get(), "must not spawn when a daemon already answers");
}

#[tokio::test]
async fn ensure_daemon_spawns_then_waits_for_ready() {
    let paths = CommsPaths {
        comms_dir: PathBuf::from("/tmp/x"),
        socket_path: PathBuf::from("/tmp/x/comms.sock"),
    };
    let alive = std::sync::atomic::AtomicBool::new(false);
    let res = ensure_daemon_with(
        &paths,
        |_| alive.load(std::sync::atomic::Ordering::SeqCst),
        |_| {
            alive.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    assert!(res.is_ok(), "daemon became ready after spawn");
}

#[test]
fn spawn_ready_timeout_covers_a_cold_full_feature_start() {
    const MINIMUM_COLD_START_BUDGET: Duration = Duration::from_secs(30);

    assert!(
        SPAWN_READY_TIMEOUT >= MINIMUM_COLD_START_BUDGET,
        "the comms daemon needs at least the agent daemon's ten-second cold-start budget"
    );
}

/// A successor spawned while its predecessor drains waits for the lock through the predecessor's
/// whole teardown, and the client that spawned it waits for the successor past that.
#[test]
fn the_waits_on_a_draining_daemon_cover_its_teardown() {
    use crate::comms::daemon::{HOLDER_DRAIN_WAIT, TEARDOWN_MAX};

    assert!(HOLDER_DRAIN_WAIT > TEARDOWN_MAX);
    assert!(SPAWN_READY_TIMEOUT > HOLDER_DRAIN_WAIT);
    assert!(
        TEARDOWN_MAX <= Duration::from_secs(30),
        "docs/systemd/basemind-comms.service stops the unit after TimeoutStopSec=30"
    );
}

/// A socket path past `sun_path` can never be bound, so the caller must be told that — not
/// left to wait out the readiness timeout and report a hang. Asserting the error is NOT
/// `SpawnTimeout` is the point: that is the misleading message this replaces.
#[cfg(unix)]
#[tokio::test]
async fn ensure_daemon_rejects_a_socket_path_past_the_platform_limit() {
    let comms_dir = PathBuf::from("/tmp").join("d".repeat(SUN_PATH_MAX));
    let paths = CommsPaths {
        socket_path: comms_socket_path(&comms_dir),
        comms_dir,
    };
    let spawned = std::cell::Cell::new(false);
    let res = ensure_daemon_with(
        &paths,
        |_| false,
        |_| {
            spawned.set(true);
            Ok(())
        },
    )
    .await;

    let err = res.expect_err("an unbindable path must fail");
    assert!(
        matches!(err, SingletonError::SocketPathTooLong { .. }),
        "must name the path-length limit, not blame a timeout: {err}"
    );
    assert!(
        err.to_string().contains("BASEMIND_COMMS_DIR"),
        "the message must say which knob to change: {err}"
    );
    assert!(!spawned.get(), "must not spawn a daemon that cannot possibly bind");
}

/// Pins the boundary rather than a comfortable middle: an off-by-one here would either reject
/// paths the kernel accepts or let through the one length that fails at `bind`.
#[cfg(unix)]
#[test]
fn the_guard_accepts_the_longest_bindable_path_and_rejects_one_byte_more() {
    let at_limit = PathBuf::from("/".to_string() + &"a".repeat(SUN_PATH_MAX - 1));
    assert_eq!(at_limit.as_os_str().len(), SUN_PATH_MAX);
    check_socket_path_len(&at_limit).expect("a path exactly at the limit is bindable");

    let over = PathBuf::from("/".to_string() + &"a".repeat(SUN_PATH_MAX));
    assert!(
        matches!(
            check_socket_path_len(&over),
            Err(SingletonError::SocketPathTooLong { .. })
        ),
        "one byte past the limit must be rejected"
    );
}

/// `BASEMIND_COMMS_DIR` names a directory the user chose; its mode is theirs. Only the generation
/// directory inside it, which basemind owns, is made owner-only.
#[cfg(unix)]
#[test]
fn an_overriding_comms_dir_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().join("shared");
    std::fs::create_dir(&base).expect("mkdir");
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let generation = super::create_generation_dir(&base, true).expect("create the generation dir");

    let mode = |path: &std::path::Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&base), 0o755, "the user's directory was chmodded");
    assert_eq!(mode(&generation), 0o700);
}
