//! Why the daemon turns a relay session down: the token and the detail its welcome carries.
//!
//! The token is what the relay client decides on. Whether it retries, and how patiently, depends
//! on whether the refusal can clear by itself, so a failure that can must not share a token with
//! one that cannot.

use super::relay::decline;
use super::workspace_pool::WorkspacePoolError;
use crate::daemon_lock;
use crate::store::{LockMeta, StoreError};

/// The token and detail for a workspace the daemon could not set up for a relay session.
///
/// Lock contention gets its own token: the index lock is held by another basemind process, most
/// often the daemon of another protocol generation serving the same repository, and is free again
/// once that process lets go. The detail then names the holder. A refused root is the same on
/// every retry and carries the guidance written for it. Anything else keeps the generic token,
/// with the error text as the detail.
pub(super) fn of_build_failure(error: &anyhow::Error) -> (&'static str, Option<String>) {
    let mut contended = false;
    for cause in error.chain() {
        if let Some(WorkspacePoolError::RootRefused { message, .. }) = cause.downcast_ref::<WorkspacePoolError>() {
            return (decline::ROOT_REFUSED, Some(message.clone()));
        }
        match cause.downcast_ref::<StoreError>() {
            Some(StoreError::Locked {
                holder: Some(holder), ..
            }) => return (decline::WORKSPACE_LOCKED, Some(describe_holder(holder))),
            Some(store) if store.is_lock_contention() => contended = true,
            _ => {}
        }
        if matches!(cause.downcast_ref::<fjall::Error>(), Some(fjall::Error::Locked)) {
            contended = true;
        }
    }
    if contended {
        (decline::WORKSPACE_LOCKED, None)
    } else {
        (decline::HOST_BUILD_FAILED, Some(chain_text(error)))
    }
}

/// The error and its causes on one line, without the causes an outer message already quotes.
fn chain_text(error: &anyhow::Error) -> String {
    let mut text = String::new();
    for cause in error.chain().map(ToString::to_string) {
        if text.is_empty() {
            text = cause;
        } else if !text.contains(&cause) {
            text.push_str(": ");
            text.push_str(&cause);
        }
    }
    text
}

/// Name the process the lock's sidecar records. A daemon is named by the version it registered
/// under, since that is what tells a person which sessions to close; any other holder by the
/// command the sidecar records.
fn describe_holder(holder: &LockMeta) -> String {
    match daemon_lock::registered_daemon(holder.pid) {
        Some(daemon) => format!("the basemind v{} daemon, pid {}", daemon.version, holder.pid),
        None => format!("`{}`, pid {}", holder.command, holder.pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(holder: Option<LockMeta>) -> anyhow::Error {
        anyhow::Error::new(WorkspacePoolError::Store(StoreError::Locked {
            path: "/cache/workspaces/k/.lock".into(),
            holder,
        }))
    }

    #[test]
    fn a_held_index_lock_is_declined_as_a_locked_workspace_naming_the_holder() {
        let (code, detail) = of_build_failure(&locked(Some(LockMeta {
            command: "basemind scan".to_string(),
            pid: 0x7FFF_FFFE,
            acquired_unix: 0,
        })));
        assert_eq!(code, decline::WORKSPACE_LOCKED);
        assert_eq!(detail.as_deref(), Some("`basemind scan`, pid 2147483646"));

        let (code, detail) = of_build_failure(&locked(None).context("build the read stack"));
        assert_eq!(
            code,
            decline::WORKSPACE_LOCKED,
            "a lock with no sidecar is still a lock"
        );
        assert_eq!(detail, None);

        let fjall = anyhow::Error::new(fjall::Error::Locked).context("open hosted git cache");
        assert_eq!(of_build_failure(&fjall).0, decline::WORKSPACE_LOCKED);
    }

    #[test]
    fn a_refused_root_and_an_unnamed_failure_keep_their_own_tokens_and_say_why() {
        let refused = anyhow::Error::new(WorkspacePoolError::RootRefused {
            root: "/".into(),
            message: "refusing to index /".to_string(),
        });
        assert_eq!(
            of_build_failure(&refused),
            (decline::ROOT_REFUSED, Some("refusing to index /".to_string()))
        );

        let other = anyhow::anyhow!("disk full").context("open hosted read stack store");
        let (code, detail) = of_build_failure(&other);
        assert_eq!(code, decline::HOST_BUILD_FAILED);
        assert_eq!(detail.as_deref(), Some("open hosted read stack store: disk full"));

        let quoted = anyhow::Error::new(WorkspacePoolError::Config(crate::config::ConfigError::MissingSchema));
        let (_, detail) = of_build_failure(&quoted);
        assert_eq!(
            detail.as_deref().map(|text| text.matches("$schema").count()),
            Some(2),
            "a cause the outer message quotes is not repeated: {detail:?}"
        );
    }
}
