//! A relay handshake the daemon answered and turned down, and what the relay does about it.

use basemind::comms::relay::decline;

use crate::stdio_relay::ReconnectFailure;

/// The daemon answered the relay handshake and turned this session down.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RelayRefusal {
    /// The daemon and this binary speak different relay protocols.
    #[error(
        "the basemind daemon (v{daemon_version}) speaks relay protocol {daemon} and this basemind \
         speaks {client}; {}",
        if daemon > client {
            "update this basemind, or stop that daemon with `basemind comms stop` once its sessions are done"
        } else {
            "the daemon is from an older build and is replaced once its sessions end; `basemind comms stop` \
             stops it now"
        }
    )]
    ProtoMismatch {
        /// The relay protocol the daemon reported.
        daemon: u32,
        /// This binary's relay protocol.
        client: u32,
        /// The daemon's build version.
        daemon_version: String,
    },
    /// The daemon declined for a reason of its own.
    #[error("{}", declined_message(.code, .detail.as_deref()))]
    Declined {
        /// The daemon's refusal token, one of [`decline`] or one a later daemon introduced.
        code: String,
        /// What the daemon added for the reader: the lock's holder, or why the root was refused.
        detail: Option<String>,
    },
}

/// The text a declined session gives the host. A locked workspace is spelled out, because the
/// person reading it may have to act. The usual holder is the daemon of another basemind version
/// that still has a session open in the same repository, but a command of this version holds the
/// same lock, so the advice covers both.
fn declined_message(code: &str, detail: Option<&str>) -> String {
    match (code, detail) {
        (decline::WORKSPACE_LOCKED, holder) => format!(
            "another basemind holds this workspace's index{}. This session connects by itself once that \
             process lets go: a daemon of another basemind version exits when its sessions are closed and it is \
             idle, a command when it finishes",
            holder.map(|holder| format!(" ({holder})")).unwrap_or_default()
        ),
        (decline::DAEMON_DRAINING, _) => "the basemind daemon is stopping; reconnecting to its successor".to_string(),
        (code, Some(detail)) => format!("the basemind daemon declined this session: {code}: {detail}"),
        (code, None) => format!("the basemind daemon declined this session: {code}"),
    }
}

impl RelayRefusal {
    /// How the relay treats the refusal. Only a refusal known to repeat unchanged ends the
    /// session: a token this binary does not know comes from a newer daemon, whose reasons may
    /// well clear, so it is retried at the long back-off and reported meanwhile.
    pub(crate) fn failure(&self) -> ReconnectFailure {
        match self {
            // A newer daemon comes back newer however often this binary retries.
            Self::ProtoMismatch { daemon, client, .. } if daemon > client => ReconnectFailure::Fatal,
            Self::ProtoMismatch { .. } => ReconnectFailure::Blocked,
            Self::Declined { code, .. } => match code.as_str() {
                // The daemon judged what this session asked for, which a retry repeats unchanged.
                // So does an index only a newer basemind can read.
                decline::VIEW_UNSUPPORTED | decline::ROOT_REFUSED | decline::INDEX_NEWER => ReconnectFailure::Fatal,
                // The daemon is on its way out and the next attempt starts its successor, or its
                // own scan holds the workspace for a moment.
                decline::DAEMON_DRAINING | decline::WORKSPACE_BUSY => ReconnectFailure::Transient,
                _ => ReconnectFailure::Blocked,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declined(code: &str, detail: Option<&str>) -> RelayRefusal {
        RelayRefusal::Declined {
            code: code.to_string(),
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn only_a_refusal_that_repeats_unchanged_ends_the_session() {
        assert_eq!(
            declined(decline::VIEW_UNSUPPORTED, None).failure(),
            ReconnectFailure::Fatal
        );
        assert_eq!(
            declined(decline::ROOT_REFUSED, Some("not a project")).failure(),
            ReconnectFailure::Fatal
        );
        assert_eq!(
            declined(decline::DAEMON_DRAINING, None).failure(),
            ReconnectFailure::Transient,
            "a draining daemon is replaced by the next attempt"
        );
        assert_eq!(
            declined(decline::WORKSPACE_LOCKED, None).failure(),
            ReconnectFailure::Blocked
        );
        assert_eq!(
            declined(decline::HOST_BUILD_FAILED, None).failure(),
            ReconnectFailure::Blocked
        );
        assert_eq!(
            declined(decline::WORKSPACE_BUSY, None).failure(),
            ReconnectFailure::Transient,
            "the daemon's own scan is over in moments"
        );
        assert_eq!(
            declined(decline::INDEX_NEWER, None).failure(),
            ReconnectFailure::Fatal,
            "an index from a newer basemind stays one"
        );
        assert_eq!(
            declined("a_token_from_a_later_daemon", None).failure(),
            ReconnectFailure::Blocked,
            "an unknown token is never fatal"
        );
    }

    /// The holder is not always another version's daemon: a `basemind scan` of this version holds
    /// the same lock. The advice has to be true for both.
    #[test]
    fn a_locked_workspace_says_who_holds_it_and_what_ends_the_wait() {
        const ADVICE: &str = "This session connects by itself once that process lets go: a daemon of another \
                              basemind version exits when its sessions are closed and it is idle, a command when \
                              it finishes";
        let daemon = declined(decline::WORKSPACE_LOCKED, Some("the basemind v0.33.0 daemon, pid 4242")).to_string();
        assert_eq!(
            daemon,
            format!("another basemind holds this workspace's index (the basemind v0.33.0 daemon, pid 4242). {ADVICE}")
        );
        let scan = declined(decline::WORKSPACE_LOCKED, Some("`basemind scan`, pid 77")).to_string();
        assert!(
            scan.contains("(`basemind scan`, pid 77)") && !scan.contains("Close that version's sessions"),
            "{scan}"
        );
        let unnamed = declined(decline::WORKSPACE_LOCKED, None).to_string();
        assert_eq!(
            unnamed,
            format!("another basemind holds this workspace's index. {ADVICE}")
        );
    }

    #[test]
    fn any_other_refusal_carries_its_token_and_the_daemons_detail() {
        assert_eq!(
            declined(decline::HOST_BUILD_FAILED, Some("disk full")).to_string(),
            "the basemind daemon declined this session: host_build_failed: disk full"
        );
        assert_eq!(
            declined(decline::VIEW_UNSUPPORTED, None).to_string(),
            "the basemind daemon declined this session: view_unsupported"
        );
    }
}
