//! A2A core domain types.
//!
//! Foundation types for the A2A (Agent-to-Agent) task system: identity
//! newtypes and agent metadata ([`types`]) plus the task state machine,
//! messages, artifacts, and task metadata ([`task_types`]). These are pure
//! domain types — no I/O, transport, or manager concerns live here.
//!
//! A2A is an EXPERIMENTAL, opt-in feature (excluded from `default` and `full`).
//! The server accepts, tracks, cancels, and streams tasks plus push-notification
//! configs; agent registration, routing, heartbeat watchdog, and task execution
//! are deliberately out of scope until the feature is built out (no dead
//! scaffolding is carried in the meantime).

pub mod bus;
pub mod push_notifications;
pub(crate) mod ssrf;
pub mod task_facade;
pub mod task_manager;
pub mod task_types;
pub mod types;
pub(crate) mod webhook;
