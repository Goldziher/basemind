//! Agent-to-agent communication substrate: multi-dimension THREADS, a per-agent inbox, and the
//! singleton broker daemon that backs them.
//!
//! A thread is a conversation addressed by at least two of `subject` / `path` (globset) /
//! `members`. Discovery is scoped — a thread is never globally visible; there is no auto-join.
//! [`ids`] holds the validated identifier newtypes that double as composite-key segments in the
//! comms store; the rest of the module adds the transport traits, the second Fjall-backed
//! `CommsStore`, the broker, and the front-ends (Unix socket, in-process, future A2A HTTP).

pub mod identity;
pub mod ids;

#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod client;
/// Second `impl CommsClient` block: the daemon-forwarding rescan / memory / governance /
/// resolved-refs / git-history RPCs, split out of `client.rs` for the line cap.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod client_forward;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod code_search_proto;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod cursor;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod daemon;
/// Second `impl Broker` block: the forwarded memory / governance / resolved-refs handlers, split
/// out of `daemon.rs` for the line cap.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_forward_handlers;
/// RAII refcount guards (`LinkGuard` / `WorkGuard`) split out of `daemon.rs` for the line cap and
/// re-exported from it so their historical `daemon::` path stays stable.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_guards;
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_handlers;
/// Second `impl Broker` block: terminal-store-failure detection and the self-shutdown it triggers,
/// split out of `daemon.rs` for the line cap.
#[cfg(all(feature = "comms", any(unix, windows)))]
mod daemon_health;
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
pub(crate) mod git_history_ops;
/// The daemon's second MCP front-end: a stateless streamable-HTTP transport (rmcp 3.0, SEP-2567)
/// hosted alongside the Unix-socket relay.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod http_frontend;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod keys;
#[cfg(all(feature = "comms", feature = "memory"))]
pub mod memory_proto;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod model;
#[cfg(all(feature = "comms", feature = "memory"))]
pub mod proposals_proto;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod protocol;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod relay;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod resolved_proto;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod scope;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod singleton;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod store;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod store_health;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod transport;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod workspace_pool;

/// Schema version for the comms store, bound to the release minor exactly like
/// `INDEX_SCHEMA_VER` and the blob `SCHEMA_VER`. A mismatch wipes the comms store and the
/// daemon rebuilds it from scratch — comms history is durable-but-disposable scratch, not a
/// source of truth.
pub const COMMS_SCHEMA_VER: u32 = crate::version::RELEASE_MINOR as u32;
