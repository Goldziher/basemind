//! Unit tests for the singleton path resolution, bind-as-lock and bring-up control flow.

use super::*;

#[test]
fn version_is_older_orders_releases_and_ignores_prerelease() {
    assert!(version_is_older("0.6.3", "0.10.0"));
    assert!(version_is_older("0.9.0", "0.10.0"));
    assert!(version_is_older("0.10.0", "0.10.1"));
    assert!(!version_is_older("0.10.0", "0.10.0"));
    assert!(!version_is_older("0.11.0", "0.10.0"));
    assert!(!version_is_older("1.0.0", "0.10.0"));
    assert!(!version_is_older("0.10.0-rc.1", "0.10.0"));
    assert!(version_is_older("0.9.0-rc.2", "0.10.0"));
}

/// Runs on every platform: the name is a pure function of the user and the directory path.
#[test]
fn the_pipe_name_is_stable_and_distinct_per_generation_directory() {
    /// The longest account name Windows allows in `USERNAME`.
    const LONGEST_USERNAME: usize = 20;
    /// `CreateNamedPipe` rejects a name longer than this, prefix included.
    const PIPE_NAME_MAX: usize = 256;

    assert_eq!(
        pipe_name("alice", Path::new("pinned-dir")),
        r"\\.\pipe\basemind-comms-alice-7c676306023f55ea",
        "every binary of a generation must compute the same name for a directory"
    );

    let ours = generation_dir(Path::new("comms"));
    let name = pipe_name("alice", &ours);
    assert_eq!(name, pipe_name("alice", &generation_dir(Path::new("comms"))));
    assert_ne!(
        name,
        pipe_name("alice", Path::new("comms")),
        "not the unscoped directory's pipe"
    );
    assert_ne!(name, pipe_name("alice", &generation_dir(Path::new("other"))));
    assert_ne!(name, pipe_name("bob", &ours));

    let deep = generation_dir(&Path::new("x").join("y".repeat(4096)));
    let longest = pipe_name(&"u".repeat(LONGEST_USERNAME), &deep);
    assert!(longest.len() <= PIPE_NAME_MAX, "{} bytes: {longest}", longest.len());
}

#[cfg(windows)]
#[test]
fn the_windows_endpoint_is_the_pipe_named_for_the_generation_directory() {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
    let one = generation_dir(Path::new(r"C:\one\comms"));
    let two = generation_dir(Path::new(r"C:\two\comms"));
    assert_eq!(comms_socket_path(&one), PathBuf::from(pipe_name(&user, &one)));
    assert_eq!(comms_socket_path(&one), comms_socket_path(&one));
    assert_ne!(comms_socket_path(&one), comms_socket_path(&two));
    assert_ne!(comms_socket_path(&one), comms_socket_path(Path::new(r"C:\one\comms")));
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
        threads: 3,
        subscribers: 0,
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
