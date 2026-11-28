#![allow(missing_docs)]

//! The `basemind` crate: the command line, the MCP adapter, the daemon and the other front ends
//! on top of [`basemind_engine`], whose modules this crate re-exports at their old paths.

pub use basemind_engine::*;

pub mod cli;
pub mod comms;
pub mod config;
pub(crate) mod dispatch;
pub mod eval;
/// `basemind serve --lead`, the agent-launch host (ADR-0040). Unix only, like launching itself.
#[cfg(all(feature = "comms", unix))]
pub mod lead;
pub mod mcp;
pub mod render;
/// `basemind sessions`: counts from the local agent transcripts of this repository (ADR-0049).
/// CLI process only.
pub mod sessions;
pub mod watcher;
