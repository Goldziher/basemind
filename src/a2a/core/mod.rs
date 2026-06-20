//! A2A core domain types.
//!
//! Foundation types for the A2A (Agent-to-Agent) task system: identity
//! newtypes and agent metadata ([`types`]) plus the task state machine,
//! messages, artifacts, and task metadata ([`task_types`]). These are pure
//! domain types — no I/O, transport, or manager concerns live here.

pub mod bus;
pub mod push_notifications;
pub mod registry;
pub mod router;
pub(crate) mod ssrf;
pub mod task_facade;
pub mod task_manager;
pub mod task_types;
pub mod types;
pub mod watchdog;
pub(crate) mod webhook;
