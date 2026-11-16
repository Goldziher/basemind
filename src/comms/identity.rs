//! Names the identity resolver of agent comms v2 shares with the rest of basemind.
//!
//! The resolver itself is [`crate::comms::v2::identity`] (ADR-0021): one function for `serve`,
//! the CLI and the hooks.

/// The environment variable that names the agent outright.
pub const AGENT_ID_ENV: &str = "BASEMIND_AGENT_ID";

/// File in a workspace's cache directory where releases up to 0.33 kept a generated agent id.
/// Nothing writes it any more; the cache GC still knows it as part of a workspace's directory.
pub use crate::store_layout::AGENT_ID_FILE;
