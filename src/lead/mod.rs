//! `basemind serve --lead`: the one process that hosts an agent-launch supervisor (ADR-0040).
//!
//! The lead's own `serve` reads the operator's user-level policy ([`policy_file`]), builds a
//! `basemind-agents` supervisor bound to this process ([`LeadHost`]) and answers `agent_launch` /
//! `agent_stop` on its own stdio. The daemon never hosts one, and nothing reaches these tools over
//! a socket: the daemon, its HTTP front-end, a plain `serve` and every CLI relay session answer them
//! as unknown tools.

mod host;
pub mod policy_file;

pub use host::{LeadHost, LeadOff, Setup};
