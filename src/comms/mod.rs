//! Agent-to-agent communication substrate: multi-dimension THREADS, a per-agent inbox, and the
//! singleton broker daemon that backs them.
//!
//! A thread is a conversation addressed by at least two of `subject` / `path` (globset) /
//! `members`. Discovery is scoped — a thread is never globally visible; there is no auto-join.
//! [`ids`] holds the validated identifier newtypes that double as composite-key segments in the
//! comms store; the rest of the module adds the transport traits, the second Fjall-backed
//! `CommsStore`, the broker, and the front-ends (Unix socket, in-process, future A2A HTTP).

pub mod ids;

#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod client;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod cursor;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod daemon;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod frontend_inproc;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod frontend_named_pipe;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod frontend_uds;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod keys;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod model;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod protocol;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod scope;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod singleton;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod store;
#[cfg(all(feature = "comms", any(unix, windows)))]
pub mod transport;

/// Schema version for the comms store, bound to the release minor exactly like
/// `INDEX_SCHEMA_VER` and the blob `SCHEMA_VER`. A mismatch wipes the comms store and the
/// daemon rebuilds it from scratch — comms history is durable-but-disposable scratch, not a
/// source of truth.
pub const COMMS_SCHEMA_VER: u32 = crate::version::RELEASE_MINOR as u32;
