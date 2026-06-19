//! Agent-to-agent communication substrate: named rooms, per-agent inbox, and the
//! singleton broker daemon that backs them.
//!
//! This module is built in phases (see `docs/agent-comms.md`). The first landed piece is
//! [`ids`] — the validated identifier newtypes that double as composite-key segments in the
//! comms store. Subsequent phases add the transport traits, the second Fjall-backed
//! `CommsStore`, the broker, and the front-ends (Unix socket, in-process, future A2A HTTP).

pub mod ids;

#[cfg(feature = "comms")]
pub mod client;
#[cfg(feature = "comms")]
pub mod cursor;
#[cfg(feature = "comms")]
pub mod daemon;
#[cfg(feature = "comms")]
pub mod frontend_inproc;
#[cfg(feature = "comms")]
pub mod frontend_uds;
#[cfg(feature = "comms")]
pub mod keys;
#[cfg(feature = "comms")]
pub mod model;
#[cfg(feature = "comms")]
pub mod protocol;
#[cfg(feature = "comms")]
pub mod scope;
#[cfg(feature = "comms")]
pub mod singleton;
#[cfg(feature = "comms")]
pub mod store;
#[cfg(feature = "comms")]
pub mod transport;

/// Schema version for the comms store, bound to the release minor exactly like
/// `INDEX_SCHEMA_VER` and the blob `SCHEMA_VER`. A mismatch wipes the comms store and the
/// daemon rebuilds it from scratch — comms history is durable-but-disposable scratch, not a
/// source of truth.
pub const COMMS_SCHEMA_VER: u32 = crate::version::RELEASE_MINOR as u32;
