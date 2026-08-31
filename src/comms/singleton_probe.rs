//! Serviceability probing and forced reclaim of a daemon that will not answer.
//!
//! A child of [`singleton`](super) because both halves lean on its private framing helpers, and
//! because the question they answer is the one the endpoint alone cannot: `probe_alive` asks
//! whether a *process* is on the other end of the socket, which a daemon holding the singleton
//! with a broken store still satisfies. [`probe_serving`] asks whether it can actually serve, and
//! [`force_terminate`] is the escalation for when it cannot and will not stand down on request.

use std::path::Path;
use std::time::Duration;

use super::super::protocol::{CommsRequest, CommsResponse};
use super::{PROBE_ATTEMPTS, PROBE_RETRY_BACKOFF, roundtrip};

/// What a serviceability probe found at a daemon's endpoint. Distinct from [`super::probe_alive`],
/// which
/// answers "is a process on the other end of this socket" — the `Ping` it sends never touches the
/// store, so a daemon whose store is wedged still answers it. This asks the harder question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonProbe {
    /// The daemon answered a `Status` request, which reads the store. It is really serving.
    Serving {
        /// Active threads it reported, as a sign of life beyond the bare reply.
        threads: u32,
    },
    /// The daemon answered, and the answer was an error: it holds the endpoint but cannot serve.
    /// This is the poisoned-store shape — every request fails identically while the process stays
    /// alive, which is exactly what a liveness-only report cannot distinguish from health.
    Failing {
        /// The stable error token (`store_error`, …).
        code: String,
        /// The daemon's rendered error.
        message: String,
    },
    /// Nothing answered within the probe's bounded timeout: no endpoint, or a hung accept loop.
    Unreachable,
}

/// Probe whether the daemon at `socket_path` is actually *serving*, not merely alive.
///
/// Sends one `Status` request — which reads the store — and classifies the reply. Bounded by the
/// same short socket timeouts as every other synchronous helper here, so this stays safe to run on
/// a wedged machine.
///
/// Retries on `Unreachable` only, reusing the shared `PROBE_ATTEMPTS` budget. A daemon that has bound
/// its socket but has not reached its accept loop yet — the first seconds after `comms start` — drops the
/// first attempt, and reporting that as "not responding" would be the same misleading verdict this
/// probe exists to prevent, just in the other direction. A `Serving` or `Failing` reply is a
/// definitive answer and returns immediately.
#[cfg(any(unix, windows))]
pub fn probe_serving(socket_path: &Path) -> DaemonProbe {
    for attempt in 0..PROBE_ATTEMPTS {
        match roundtrip(socket_path, &CommsRequest::Status) {
            Some(CommsResponse::Status(report)) => {
                return DaemonProbe::Serving {
                    threads: report.threads,
                };
            }
            Some(CommsResponse::Error { code, message }) => return DaemonProbe::Failing { code, message },
            // Any other variant is a protocol-shape surprise (a skewed daemon answering something
            // else). Reporting it as failing rather than serving keeps the verdict conservative. ~keep
            Some(other) => {
                return DaemonProbe::Failing {
                    code: "unexpected_response".to_string(),
                    message: format!("daemon answered Status with {other:?}"),
                };
            }
            None if attempt + 1 < PROBE_ATTEMPTS => std::thread::sleep(PROBE_RETRY_BACKOFF),
            None => {}
        }
    }
    DaemonProbe::Unreachable
}

#[cfg(not(any(unix, windows)))]
pub fn probe_serving(_socket_path: &Path) -> DaemonProbe {
    DaemonProbe::Unreachable
}

/// How a daemon answered a `Stop` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// It accepted and is draining.
    Accepted,
    /// It answered, and the answer was a deliberate refusal — `daemon_busy` above all, which the
    /// broker returns while persistent clients or work are still attached. An informed "no" from a
    /// daemon that is plainly serving must be reported, never escalated: killing it is exactly the
    /// severing that refusal exists to prevent.
    Refused {
        /// The stable refusal token.
        code: String,
        /// The daemon's message.
        message: String,
    },
    /// Nothing answered, so there is no refusal to respect and reclaim is on the table.
    Unreachable,
}

impl StopOutcome {
    /// Whether a daemon in this state may be terminated by pid if it does not go away on its own.
    ///
    /// A `store_error` refusal is not a refusal in the meaningful sense: the daemon did answer, but
    /// only to say its store is broken, which is the wedge this reclaim path exists for.
    pub fn permits_reclaim(&self) -> bool {
        match self {
            Self::Accepted | Self::Unreachable => true,
            Self::Refused { code, .. } => code == "store_error",
        }
    }
}

/// Ask a daemon to stop and classify its answer. Unlike [`request_stop`](super::request_stop),
/// which discards the reply, this keeps the distinction that decides whether escalating to the pid
/// is legitimate.
pub fn request_stop_classified(socket_path: &Path) -> StopOutcome {
    match roundtrip(socket_path, &CommsRequest::Stop) {
        Some(CommsResponse::Ok) => StopOutcome::Accepted,
        Some(CommsResponse::Error { code, message }) => StopOutcome::Refused { code, message },
        Some(other) => StopOutcome::Refused {
            code: "unexpected_response".to_string(),
            message: format!("daemon answered Stop with {other:?}"),
        },
        None => StopOutcome::Unreachable,
    }
}

/// Terminate a daemon that will not answer its own `Stop` RPC, addressing it by pid.
///
/// The documented recovery path (`comms stop`) asks the running daemon to drain — a request that a
/// daemon with a broken store can refuse, which routes recovery through the exact subsystem that is
/// down. This is the escalation: SIGTERM (which the daemon's signal handler turns into the same
/// clean drain), then SIGKILL if it is still there after `grace`. Returns whether the process is
/// gone by the time we return.
#[cfg(unix)]
pub fn force_terminate(pid: u32, grace: Duration) -> bool {
    use crate::daemon_lock::pid_is_live;

    if !pid_is_live(pid) {
        return true;
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if !pid_is_live(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !pid_is_live(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !pid_is_live(pid)
}

/// [`force_terminate`] on Windows. There is no SIGTERM, so this goes straight to `taskkill /F`,
/// which is the platform's equivalent of the SIGKILL fallback.
#[cfg(windows)]
pub fn force_terminate(pid: u32, _grace: Duration) -> bool {
    use crate::daemon_lock::pid_is_live;

    if !pid_is_live(pid) {
        return true;
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !pid_is_live(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !pid_is_live(pid)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::comms::protocol::PROTO_VER;

    /// Serve exactly one framed `CommsOut` reply on a fresh socket, returning its path and the
    /// server thread. Mirrors the real UDS front-end's framing (see the test above).
    #[cfg(unix)]
    fn serve_one_reply(
        dir: &std::path::Path,
        name: &str,
        reply: crate::comms::protocol::CommsOut,
    ) -> (PathBuf, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let socket = dir.join(name);
        let listener = UnixListener::bind(&socket).expect("bind");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut prefix = [0u8; 4];
            stream.read_exact(&mut prefix).expect("read len");
            let len = u32::from_be_bytes(prefix) as usize;
            let mut req = vec![0u8; len];
            stream.read_exact(&mut req).expect("read req");
            let body = rmp_serde::to_vec_named(&reply).expect("encode");
            let out_len = u32::try_from(body.len()).expect("len fits");
            stream.write_all(&out_len.to_be_bytes()).expect("write len");
            stream.write_all(&body).expect("write body");
        });
        (socket, handle)
    }

    #[cfg(unix)]
    #[test]
    fn probe_serving_separates_a_serving_daemon_from_one_that_only_holds_the_socket() {
        use crate::comms::protocol::{CommsOut, CommsResponse, StatusReport};

        let dir = tempfile::tempdir().expect("tempdir");

        let (ok_socket, ok_server) = serve_one_reply(
            dir.path(),
            "serving.sock",
            CommsOut::Response(CommsResponse::Status(StatusReport {
                pid: 1,
                version: "0.25.1".to_string(),
                build_id: "abc".to_string(),
                proto_ver: PROTO_VER,
                uptime_secs: 1,
                threads: 7,
                subscribers: 0,
            })),
        );
        assert_eq!(
            probe_serving(&ok_socket),
            DaemonProbe::Serving { threads: 7 },
            "a daemon that answers Status is really serving"
        );
        ok_server.join().expect("server thread");

        // The poisoned-store shape: the process is alive, the socket answers, and every request
        // comes back as the same store error. Liveness alone cannot tell this from health. ~keep
        let (bad_socket, bad_server) = serve_one_reply(
            dir.path(),
            "wedged.sock",
            CommsOut::Response(CommsResponse::Error {
                code: "store_error".to_string(),
                message: "fjall error: FjallError: Poisoned".to_string(),
            }),
        );
        let verdict = probe_serving(&bad_socket);
        bad_server.join().expect("server thread");
        match verdict {
            DaemonProbe::Failing { code, message } => {
                assert_eq!(code, "store_error");
                assert!(
                    message.contains("Poisoned"),
                    "the daemon's own error is carried through"
                );
            }
            other => panic!("a daemon refusing every request must not read as healthy: {other:?}"),
        }

        assert_eq!(
            probe_serving(&dir.path().join("nothing-here.sock")),
            DaemonProbe::Unreachable,
            "no endpoint is not reachable"
        );
    }
}
