//! `basemind scan` next to a running daemon (ADR-0024 rule 5): the daemon is the workspace's writer,
//! so a working-tree scan is handed to it as a full `Rescan` instead of colliding with its lock.
//!
//! The scan is forwarded when this generation's daemon answers and the workspace writer lock is
//! free (the daemon can host it) or held by that daemon. Nothing here starts a daemon. With no
//! daemon, or a lock held by any other process, the direct scan runs as before and reports the
//! holder. A flag the `Rescan` request cannot carry is refused rather than dropped.

use super::*;

/// `scan` flags the forwarded request honours. `--no-git-history` is honoured by not asking the
/// daemon to sync the history index afterwards; the rest are global flags no scan forwards.
const FORWARDED_OR_GLOBAL: [&str; 7] = ["no_git_history", "root", "quiet", "verbose", "no_color", "json", "view"];

/// The `scan` flags set on the command line that a forwarded scan cannot honour, as `--name`, and
/// the environment variables standing in for such flags.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Unforwardable {
    pub(super) flags: Vec<String>,
    pub(super) env: Vec<String>,
}

/// Sort the `scan` arguments the user set into the [`Unforwardable`] ones, by where each value came
/// from. A default is not a choice, and a `BASEMIND_DOCUMENTS_*` / `BASEMIND_LLM_*` variable is
/// reported apart because the daemon reads its own environment, not this shell's.
pub(super) fn unforwardable(matches: &clap::ArgMatches) -> Unforwardable {
    let command = Cli::command();
    let Some(scan) = command.find_subcommand("scan") else {
        return Unforwardable::default();
    };
    let mut found = Unforwardable::default();
    for arg in scan.get_arguments() {
        let id = arg.get_id().as_str();
        if FORWARDED_OR_GLOBAL.contains(&id) {
            continue;
        }
        match matches.value_source(id) {
            Some(clap::parser::ValueSource::CommandLine) => {
                found.flags.push(format!("--{}", arg.get_long().unwrap_or(id)));
            }
            Some(clap::parser::ValueSource::EnvVariable) => {
                if let Some(name) = arg.get_env() {
                    found.env.push(name.to_string_lossy().into_owned());
                }
            }
            _ => {}
        }
    }
    found
}

/// The message refusing a forwarded scan the user gave flags it cannot carry.
fn refusal(pid: u32, flags: &[String]) -> String {
    format!(
        "the basemind daemon (pid {pid}) hosts this workspace, so `basemind scan` is forwarded to it, and the \
         forwarded scan cannot apply {}. Drop {}, put document options in basemind.toml, or stop the daemon \
         (`basemind comms stop`) and scan directly.",
        flags.join(", "),
        if flags.len() == 1 { "it" } else { "them" }
    )
}

/// What a `scan` does about this generation's daemon.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(super) enum Route {
    /// No daemon of this generation can take the scan: scan directly.
    Direct,
    /// The daemon at `pid` hosts the workspace, or can: it holds the writer lock (`holds_lock`) or
    /// the lock is free.
    Daemon {
        client: Box<basemind::comms::client::CommsClient>,
        pid: u32,
        holds_lock: bool,
    },
}

/// Find out whether this generation's daemon can take a scan of `root`. Never starts a daemon,
/// never takes over one, and never fails: a daemon that cannot be reached or asked is no daemon.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(super) async fn route(root: &std::path::Path) -> Route {
    use basemind::comms::client::{CommsClient, scope_context_for};

    if !basemind::cli::rescan::daemon_is_up() {
        return Route::Direct;
    }
    let Ok(paths) = basemind::comms::singleton::resolve_paths() else {
        return Route::Direct;
    };
    let agent = basemind::comms::identity::cli_agent_id(root);
    let (remote, cwd) = scope_context_for(root);
    let connected = CommsClient::connect_with_respawn(&paths, agent, remote, cwd, |_| {
        Err(std::io::Error::other("a scan does not start a daemon"))
    })
    .await;
    let mut client = match connected {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(%error, "scan: the daemon did not take a connection; scanning directly");
            return Route::Direct;
        }
    };
    let pid = match client.status().await {
        Ok(status) => status.pid,
        Err(error) => {
            tracing::debug!(%error, "scan: the daemon did not report its pid; scanning directly");
            return Route::Direct;
        }
    };
    let holds_lock = match basemind::store::probe_writer_lock(&basemind::store::workspace_cache_dir(root)) {
        basemind::store::WriterProbe::Free => false,
        basemind::store::WriterProbe::Held { holder: Some(meta) } if meta.pid == pid => true,
        basemind::store::WriterProbe::Held { .. } => return Route::Direct,
    };
    Route::Daemon {
        client: Box::new(client),
        pid,
        holds_lock,
    }
}

/// Refuse `--staged` / `--rev` (`flag`) while this generation's daemon holds the workspace: those
/// views are written under the same lock, and the daemon scans the working tree only.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(super) fn refuse_view_scan_under_daemon(root: &std::path::Path, flag: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    if let Route::Daemon {
        pid, holds_lock: true, ..
    } = runtime.block_on(route(root))
    {
        return Err(basemind::cli::exit::CliExit::busy(format!(
            "the basemind daemon (pid {pid}) holds this workspace's index, and `{flag}` cannot be forwarded to it: \
             the daemon scans the working tree only. Stop the daemon (`basemind comms stop`) and rerun."
        )));
    }
    Ok(())
}

/// Forward a working-tree scan to the daemon when [`route`] finds one. `Ok(false)` means scan
/// directly. Exit codes: `2` for a flag the daemon cannot apply, `3` when the daemon is stopping,
/// `130` on Ctrl-C (the daemon finishes the scan it was given), `1` for a failed scan.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(super) fn forward_scan(
    root: &std::path::Path,
    args: &ScanArgs,
    matches: &clap::ArgMatches,
    cancel: &basemind::scanner::ScanCancel,
    out: &mut impl std::io::Write,
) -> Result<bool> {
    use basemind::git_history::proto::{GitHistoryOp, GitHistoryReply};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async {
        let Route::Daemon { mut client, pid, .. } = route(root).await else {
            return Ok(false);
        };
        let unforwardable = unforwardable(matches);
        if !unforwardable.flags.is_empty() {
            return Err(basemind::cli::exit::CliExit::usage(refusal(pid, &unforwardable.flags)));
        }
        if !unforwardable.env.is_empty() {
            tracing::warn!(
                vars = %unforwardable.env.join(", "),
                "scan is forwarded to the daemon, which reads its own environment and basemind.toml, not these"
            );
        }
        let report = tokio::select! {
            report = client.rescan_as(root.to_path_buf(), None, true, true, true) => report,
            () = until_cancelled(cancel) => {
                return Err(basemind::cli::exit::CliExit::interrupted(
                    "scan interrupted: the daemon finishes the scan it was given in the background",
                ));
            }
        }
        .map_err(|error| forwarded_failure(pid, error))?;
        super::main_rescan::print_forwarded(out, "scan", &report);
        if !args.no_git_history && basemind::git_history::index_enabled() && basemind::git::Repo::discover(root).is_ok()
        {
            match client.git_history(root.to_path_buf(), GitHistoryOp::Sync).await {
                Ok(GitHistoryReply::Synced(outcome)) => {
                    let _ = writeln!(out, "git-history index: {}", synced(&outcome));
                }
                Ok(_) => tracing::warn!("git-history sync: unexpected daemon reply"),
                Err(error) => tracing::warn!(%error, "git-history sync via the daemon failed"),
            }
        }
        Ok(true)
    })
}

/// Resolve once `cancel` trips (Ctrl-C).
#[cfg(all(feature = "comms", any(unix, windows)))]
async fn until_cancelled(cancel: &basemind::scanner::ScanCancel) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The CLI error for a forwarded scan the daemon did not complete. A daemon that is draining
/// refused or cut the scan short; that is a retry, not a failure.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn forwarded_failure(pid: u32, error: basemind::comms::client::CommsClientError) -> anyhow::Error {
    use basemind::comms::client::CommsClientError;

    match error {
        CommsClientError::Broker { code, message } if code == "rescan_draining" || code == "rescan_cancelled" => {
            basemind::cli::exit::CliExit::busy(format!(
                "the basemind daemon (pid {pid}) is stopping and did not complete the scan ({message}); rerun \
                 `basemind scan`"
            ))
        }
        other => anyhow::anyhow!("daemon scan failed: {other}"),
    }
}

/// The summary line for a history sync, worded as the direct scan words it.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn synced(outcome: &basemind::git_history::proto::SyncOutcome) -> String {
    use basemind::git_history::proto::SyncOutcome;

    match outcome {
        SyncOutcome::Fresh => "up to date".to_string(),
        SyncOutcome::Incremental { added } => format!("+{added} commits"),
        SyncOutcome::FullRebuild { reason, commits } => format!("rebuilt {commits} commits ({reason})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_matches(argv: &[&str]) -> clap::ArgMatches {
        let matches = Cli::command()
            .try_get_matches_from(argv.iter().copied())
            .expect("parse");
        matches.subcommand_matches("scan").expect("scan").clone()
    }

    #[test]
    fn only_flags_given_on_the_command_line_that_a_rescan_cannot_carry_are_unforwardable() {
        let plain = scan_matches(&["basemind", "--quiet", "scan", "--no-git-history"]);
        assert_eq!(unforwardable(&plain).flags, Vec::<String>::new());

        let given = scan_matches(&[
            "basemind",
            "scan",
            "--rebuild-git-history",
            "--verify-shared-base",
            "--documents-enabled",
            "false",
        ]);
        assert_eq!(
            unforwardable(&given).flags,
            ["--verify-shared-base", "--rebuild-git-history", "--documents-enabled"]
        );
    }

    #[test]
    fn the_refusal_names_every_flag_and_the_way_out() {
        let message = refusal(7, &["--rebuild-git-history".to_string()]);
        assert!(message.contains("pid 7"), "{message}");
        assert!(message.contains("--rebuild-git-history"), "{message}");
        assert!(message.contains("basemind comms stop"), "{message}");
    }
}
