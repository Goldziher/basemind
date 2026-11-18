//! The singleton daemon and its clients: agent messaging (ADR-0021, in [`v2`] and
//! [`agents_hub`]), the hot workspaces it hosts for `basemind serve`, the forwarded index work, and
//! the front-ends (Unix socket or named pipe, in-process, streamable HTTP).

pub mod identity;
pub mod ids;

/// The back-off, connection cap and first-byte deadline every accept loop shares.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod accept_gate;
/// The daemon's file-descriptor budget and the `RLIMIT_NOFILE` raise at start.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod fd_budget;
/// Opening the daemon's named pipe only when this user's process serves it.
#[cfg(all(feature = "comms", windows))]
pub mod pipe_owner;

/// The daemon's agent store, wait and session count (ADR-0021).
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod agents_hub;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) mod blob_gc_gate;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod client;
/// The agent requests of a client, and the daemon's agent store as an `AgentOps` for the CLI.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod client_agents;
/// Second `impl CommsClient` block: the daemon-forwarding rescan and git-history RPCs, split out of
/// `client.rs` for the line cap.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod client_forward;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod daemon;
/// Second `impl Broker` block: `Hello` and the agent requests, split out of `daemon.rs`.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_agents;
/// RAII refcount guards (`LinkGuard` / `WorkGuard`) split out of `daemon.rs` for the line cap and
/// re-exported from it so their historical `daemon::` path stays stable.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_guards;
/// Second `impl Broker` block: the streamable-HTTP per-request seam (activity guard, workspace
/// read-stack resolution, connection accounting), split out of `daemon.rs` for the line cap.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod daemon_http;
/// Lifecycle admission, the drain machinery, and non-destructive user-stop handling, split out for
/// the line cap.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_lifecycle;
/// The daemon lock/registry now lives at [`crate::daemon_lock`] and is shared with the other daemon
/// families; re-exported here so the broker's historical `comms::daemon_lock::` path stays stable.
pub use crate::daemon_lock;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod frontend_inproc;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod frontend_named_pipe;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod frontend_uds;
/// The daemon's git-history half: the pool of open per-repo indexes + the forwarded-op handler.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod generation_cleanup;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) mod git_history_ops;
/// The opt-in gate and bearer-token auth guarding [`http_frontend`].
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod http_auth;
/// The daemon's second MCP front-end: a stateless streamable-HTTP transport (rmcp 3.0, SEP-2567)
/// hosted alongside the Unix-socket relay.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod http_frontend;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod protocol;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod relay;
/// Dialing the daemon's relay endpoint.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod relay_connect;
/// The token and detail the daemon declines a relay session with.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod relay_decline;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod singleton;
#[cfg(all(feature = "comms", any(unix, windows)))]
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod transport;
pub mod v2;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod workspace_pool;

/// A fresh, collision-resistant key for one relayed request (pid + clock + counter + a per-process
/// random seed; no crypto strength needed, only uniqueness per agent).
#[cfg(all(feature = "comms", any(unix, windows)))]
pub fn new_idempotency_key() -> String {
    use std::hash::{BuildHasher as _, Hasher as _};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        hasher.finish()
    )
}
