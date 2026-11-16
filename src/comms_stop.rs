//! `basemind comms stop` and `comms stop --all`: ask a daemon to stop, wait for it to go, and
//! reclaim its generation if it has stopped answering.
//!
//! Both commands end in [`singleton::reclaim_holder`], the same steps a takeover uses, so a daemon
//! is only ever signalled after the exit window, after it was asked once more and stayed silent,
//! and only as the process its generation's `daemon.pid` names at that moment.

use std::time::Duration;

use anyhow::{Context, Result};
use basemind::comms::client::{CommsClient, CommsClientError};
use basemind::comms::singleton::{self, CommsPaths, Liveness, ReclaimRules, Reclaimed, StopOutcome};
use basemind::daemon_lock::{self, DaemonKind};

/// The one refusal that is a symptom, not a decision: the daemon answers only to say its store is
/// broken, so it does not protect itself from being reclaimed by an operator.
const STORE_ERROR: &str = "store_error";

/// Hard bound on the `Stop` RPC itself — connect plus request. A daemon wedged before its accept
/// loop never completes the connect, which used to hang `basemind comms stop` indefinitely: the
/// documented recovery path blocked on the very daemon it was meant to reclaim.
const STOP_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a daemon has to leave after a `Stop` before it is asked again. Generous relative to a
/// drain that is actually progressing (the daemon's own drain grace is 10s), so a busy-but-healthy
/// daemon is never signalled out from under an in-flight request.
const STOP_GRACE: Duration = Duration::from_secs(12);
/// How long the `Stop` sent after [`STOP_GRACE`] waits for an answer: longer than a liveness
/// probe, because a daemon still serving can be slow and must not be taken for a silent one.
const STOP_REPROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a silent daemon gets on SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(3);

/// What the first `Stop` established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    /// The daemon took the request.
    Accepted,
    /// The daemon answered that its store is broken.
    BrokenStore,
    /// Nothing answered in time.
    Unanswered,
}

impl Asked {
    fn rules(self) -> ReclaimRules {
        ReclaimRules {
            exit: STOP_GRACE,
            term_grace: TERM_GRACE,
            probe_timeout: STOP_REPROBE_TIMEOUT,
            store_error_is_silence: self == Self::BrokenStore,
        }
    }
}

/// `basemind comms stop`: stop this comms directory's daemon.
pub(crate) fn cmd_comms_stop(json: bool) -> Result<()> {
    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let (asked, reclaimed) = runtime.block_on(async {
        let asked = ask_to_stop(&paths).await?;
        anyhow::Ok((asked, singleton::reclaim_holder(&paths, None, asked.rules()).await))
    })?;
    let forced = match reclaimed {
        Reclaimed::Gone { forced } => forced,
        left => anyhow::bail!("stop: {}", why_left_running(&paths, &left)),
    };
    let accepted = asked == Asked::Accepted;
    if json {
        println!(
            "{}",
            serde_json::json!({ "stopped": accepted, "force_terminated": u8::from(forced) })
        );
    } else if forced {
        println!("comms daemon did not answer; terminated it by pid");
    } else if accepted {
        println!("comms daemon stopping");
    } else {
        println!("no live comms daemon for {}", paths.comms_dir.display());
    }
    Ok(())
}

/// Send the first `Stop`, through a full client so that a healthy daemon sees an ordinary request.
///
/// The whole RPC is bounded and its failure is not always fatal. A daemon that cannot answer is
/// precisely the one an operator is trying to stop, and it fails in two ways that both used to
/// defeat `stop`: a poisoned store fails the Hello handshake, so the command aborted before doing
/// anything; and a daemon wedged before its accept loop never completes `connect` at all, so the
/// command hung indefinitely. A daemon that answered with a refusal made an informed decision —
/// `daemon_busy` means persistent clients are still attached — so that is reported and nothing
/// more is done.
async fn ask_to_stop(paths: &CommsPaths) -> Result<Asked> {
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let agent = basemind::comms::identity::cli_agent_id(&root);
    let rpc = tokio::time::timeout(STOP_RPC_TIMEOUT, async {
        CommsClient::connect(paths, agent, None, None).await?.stop().await
    })
    .await;
    match rpc {
        Ok(Ok(())) => Ok(Asked::Accepted),
        // A daemon from another build fails the handshake before `Stop` is ever sent, yet stopping
        // it is what the takeover errors tell the operator to do. Ask without the handshake; its
        // answer is honoured the same way.
        Ok(Err(error)) if is_proto_skew(&error) => stop_without_handshake(&paths.socket_path),
        Ok(Err(error)) if is_broken_store_error(&error) => {
            tracing::warn!(%error, "comms: daemon reports a broken store; it may be reclaimed by pid");
            Ok(Asked::BrokenStore)
        }
        Ok(Err(error)) => Err(anyhow::anyhow!("stop: {error}")),
        Err(_) => {
            tracing::warn!(
                timeout_secs = STOP_RPC_TIMEOUT.as_secs(),
                "comms: daemon did not answer the stop RPC in time"
            );
            Ok(Asked::Unanswered)
        }
    }
}

/// Ask a daemon that refused the handshake to stop, by the takeover rules: its refusal is final.
/// The answer is awaited for the full [`STOP_RPC_TIMEOUT`]: a daemon slow to answer is still
/// serving sessions.
fn stop_without_handshake(socket_path: &std::path::Path) -> Result<Asked> {
    match singleton::request_stop_within(socket_path, STOP_RPC_TIMEOUT) {
        StopOutcome::Accepted => Ok(Asked::Accepted),
        StopOutcome::Refused { code, .. } if code == STORE_ERROR => Ok(Asked::BrokenStore),
        StopOutcome::Refused { code, message } => Err(anyhow::anyhow!("stop: daemon refused [{code}]: {message}")),
        StopOutcome::Unreachable => after_unanswered_stop(singleton::probe_liveness(socket_path)),
    }
}

/// What a `Stop` nobody answered means, given what a `Ping` then established. Only a daemon that
/// was asked and stayed silent goes on to be reclaimed; one that answers, or that could not be
/// asked, is left running.
fn after_unanswered_stop(ping: Liveness) -> Result<Asked> {
    match ping {
        Liveness::Silent => Ok(Asked::Unanswered),
        Liveness::Answers => Err(anyhow::anyhow!(
            "stop: the daemon did not answer the stop request within {}s but still answers pings, \
             so it was left running; retry, or terminate its process yourself if it is stuck",
            STOP_RPC_TIMEOUT.as_secs()
        )),
        Liveness::Unknown => Err(anyhow::anyhow!(
            "stop: the daemon did not answer the stop request within {}s and could not be pinged, so \
             whether it is running is not known and it was left alone; retry",
            STOP_RPC_TIMEOUT.as_secs()
        )),
    }
}

/// Why a daemon was left running, and what the operator can do about it.
fn why_left_running(paths: &CommsPaths, left: &Reclaimed) -> String {
    let dir = paths.comms_dir.display();
    match left {
        Reclaimed::Answers {
            stop: StopOutcome::Refused { code, message },
        } => format!("the daemon for {dir} is still running and refused to stop [{code}]: {message}"),
        Reclaimed::Answers { .. } => format!(
            "the daemon for {dir} was asked to stop and is still running after {}s, but it still answers, so it \
             was not signalled; retry, or terminate its process yourself if it is stuck (`basemind comms \
             doctor` lists it)",
            STOP_GRACE.as_secs()
        ),
        Reclaimed::Unidentified => format!(
            "something still holds the daemon lock or socket in {dir} and answers nothing, but {} does not \
             name a live comms daemon that holds the lock, so nothing was signalled; find the process \
             (`lsof {}` shows the lock's holder, `basemind comms doctor` the registered daemons), terminate \
             it yourself, then retry",
            paths.comms_dir.join("daemon.pid").display(),
            paths.comms_dir.join("daemon.lock").display(),
        ),
        Reclaimed::Survived { pid } => format!("the daemon for {dir} (pid {pid}) is still running after SIGKILL"),
        Reclaimed::Unknown => format!(
            "the daemon for {dir} did not answer the stop request and could not be pinged, so whether it is \
             running is not known and nothing was signalled; retry"
        ),
        Reclaimed::Gone { .. } => format!("the daemon for {dir} is gone"),
    }
}

/// Whether the handshake failed because the daemon speaks another protocol version. A daemon
/// answers a mismatched `Hello` with a `proto_skew` error, which reaches the client as a broker
/// error; `ProtoSkew` is the client's own check of a `Welcome` and covers a daemon that sent one.
fn is_proto_skew(error: &CommsClientError) -> bool {
    match error {
        CommsClientError::ProtoSkew { .. } => true,
        CommsClientError::Broker { code, .. } => code == "proto_skew",
        _ => false,
    }
}

/// Whether a failed `Stop` came back as the broker's `store_error`.
fn is_broken_store_error(error: &CommsClientError) -> bool {
    matches!(error, CommsClientError::Broker { code, .. } if code == STORE_ERROR)
}

/// `basemind comms stop --all`: ask every live comms daemon of this protocol generation on this
/// machine to drain, addressing each by its own socket, and reclaim the ones that have stopped
/// answering. Daemons of other generations are out of reach by design: their own version's
/// `comms stop --all` stops them. Uses the low-level [`singleton::request_stop_within`] rather than
/// a `CommsClient` (which could respawn the very daemon it meant to stop), with the stop command's
/// own timeout: a daemon slow to answer is serving, and a liveness probe's patience would write it
/// off as unreachable.
///
/// A daemon that answered `daemon_busy` made an informed decision to protect the persistent
/// clients attached to it, and is reported, never reclaimed. One that answered `store_error`, or
/// nothing, goes through [`singleton::reclaim_holder`] like a single `stop`. The command fails
/// when any daemon is still running afterwards, whether it refused or could not be reclaimed, and
/// names each with its reason, as a single `stop` does for its one daemon.
pub(crate) fn cmd_comms_stop_all(json: bool) -> Result<()> {
    // Comms-only: this addresses each holder over the comms stop protocol, which another daemon
    // family in the registry does not speak.
    let daemons = daemon_lock::live_daemons_of(DaemonKind::Comms);
    // One thread per daemon, so that eight slow daemons take one exit window and not eight.
    let outcomes: Vec<Result<Reclaimed, (String, String)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = daemons
            .iter()
            .map(|record| scope.spawn(|| stop_one(&record.dir)))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap_or(Ok(Reclaimed::Unidentified)))
            .collect()
    });

    let mut stopped = 0usize;
    let mut forced = 0usize;
    let mut refused = Vec::new();
    let mut left = Vec::new();
    for (record, outcome) in daemons.iter().zip(outcomes) {
        match outcome {
            Ok(Reclaimed::Gone { forced: by_pid }) => {
                stopped += 1;
                forced += usize::from(by_pid);
            }
            Ok(other) => {
                let paths = paths_of(&record.dir);
                left.push((record.pid, why_left_running(&paths, &other)));
            }
            Err((code, message)) => refused.push((record.pid, code, message)),
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "stopped": stopped,
                "force_terminated": forced,
                "refused": refused
                    .iter()
                    .map(|(pid, code, message)| serde_json::json!({
                        "pid": pid,
                        "code": code,
                        "message": message,
                    }))
                    .collect::<Vec<_>>(),
                "left_running": left
                    .iter()
                    .map(|(pid, reason)| serde_json::json!({ "pid": pid, "reason": reason }))
                    .collect::<Vec<_>>(),
            })
        );
    } else if daemons.is_empty() {
        println!("no live basemind daemons to stop");
    } else {
        println!("asked {} daemon(s) to stop", daemons.len());
        if forced > 0 {
            println!("  force-terminated {forced} daemon(s) that did not answer the stop request");
        }
        for (pid, code, message) in &refused {
            println!("  pid={pid} refused [{code}]: {message}");
            println!("    left running — stopping it would disconnect the clients it is serving");
        }
        for (pid, reason) in &left {
            println!("  pid={pid} left running: {reason}");
        }
    }
    let still_running: Vec<String> = refused
        .iter()
        .map(|(pid, code, message)| format!("pid={pid} refused [{code}]: {message}"))
        .chain(left.iter().map(|(pid, reason)| format!("pid={pid}: {reason}")))
        .collect();
    anyhow::ensure!(
        still_running.is_empty(),
        "{} daemon(s) are still running: {}",
        still_running.len(),
        still_running.join("; ")
    );
    Ok(())
}

fn paths_of(dir: &std::path::Path) -> CommsPaths {
    CommsPaths {
        comms_dir: dir.to_path_buf(),
        socket_path: singleton::comms_socket_path(dir),
    }
}

/// Stop the daemon that owns `dir`. `Err` carries the code and message of a refusal to honour.
fn stop_one(dir: &std::path::Path) -> Result<Reclaimed, (String, String)> {
    let paths = paths_of(dir);
    let asked = match singleton::request_stop_within(&paths.socket_path, STOP_RPC_TIMEOUT) {
        StopOutcome::Accepted => Asked::Accepted,
        StopOutcome::Refused { code, .. } if code == STORE_ERROR => Asked::BrokenStore,
        StopOutcome::Refused { code, message } => return Err((code, message)),
        StopOutcome::Unreachable => Asked::Unanswered,
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return Ok(Reclaimed::Unidentified);
    };
    Ok(runtime.block_on(singleton::reclaim_holder(&paths, None, asked.rules())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs on every platform. A daemon goes on to be reclaimed, and so possibly signalled, only
    /// when it was pinged and stayed silent.
    #[test]
    fn only_a_daemon_that_was_pinged_and_stayed_silent_goes_on_to_be_reclaimed() {
        assert_eq!(
            after_unanswered_stop(Liveness::Silent).expect("reclaim"),
            Asked::Unanswered
        );
        let answers = after_unanswered_stop(Liveness::Answers).expect_err("left running");
        assert!(answers.to_string().contains("still answers pings"), "{answers}");
        let unknown = after_unanswered_stop(Liveness::Unknown).expect_err("left alone");
        assert!(unknown.to_string().contains("could not be pinged"), "{unknown}");
    }

    #[test]
    fn a_daemon_that_could_not_be_asked_is_reported_as_left_alone() {
        let paths = paths_of(std::path::Path::new("comms/g1"));
        let why = why_left_running(&paths, &Reclaimed::Unknown);
        assert!(why.contains("nothing was signalled"), "{why}");
    }
}
