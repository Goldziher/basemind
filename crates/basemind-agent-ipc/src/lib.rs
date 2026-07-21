//! `basemind-agent-ipc` — cross-process transport for the `basemind-agent` engine.
//!
//! This crate mirrors basemind's own comms UDS seam (`src/comms/`): length-delimited msgpack frames
//! (`[u32 big-endian length][msgpack body]`, capped at [`MAX_FRAME_BYTES`]) over a Unix socket. Both
//! halves speak the engine's existing serde wire types ([`AgentCommand`](basemind_agent::AgentCommand)
//! / [`AgentEvent`](basemind_agent::AgentEvent)) — nothing new is serialized.
//!
//! - [`UdsAgentClient`] is the front-end half: a cross-process
//!   [`AgentClient`](basemind_agent::AgentClient) a UI drives exactly like the in-process one, so the
//!   ratatui event loop is unchanged.
//! - [`serve_connection`] is the daemon half: it bridges one accepted socket to an in-process engine
//!   (any `impl AgentClient` — in practice the [`InProcAgentClient`](basemind_agent::InProcAgentClient)
//!   returned by [`in_proc_channel`](basemind_agent::in_proc_channel)), pumping commands in and events
//!   out until either side closes.
//!
//! The engine and the UI are untouched: the same values that cross an in-process channel today cross a
//! socket here, because [`AgentCommand`](basemind_agent::AgentCommand) /
//! [`AgentEvent`](basemind_agent::AgentEvent) are already serde.

mod error;
mod frame;
// The sockets are Unix-only in this slice (mirroring the `unix`-gated front-end tests); the comms
// crate's named-pipe path is the template for a later Windows port. ~keep
#[cfg(unix)]
mod client;
#[cfg(unix)]
mod lifecycle;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod socket;

pub use error::IpcError;
pub use frame::MAX_FRAME_BYTES;

#[cfg(unix)]
pub use client::UdsAgentClient;
#[cfg(unix)]
pub use lifecycle::{ensure_daemon, ensure_daemon_with, spawn_detached};
#[cfg(unix)]
pub use server::{serve, serve_connection};
#[cfg(unix)]
pub use socket::{agent_socket_path, bind_listener, probe_alive};
