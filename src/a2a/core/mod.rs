//! A2A core domain types.
//!
//! Foundation types for the A2A (Agent-to-Agent) task system: identity
//! newtypes and agent metadata ([`types`]) plus the task state machine,
//! messages, artifacts, and task metadata ([`task_types`]). These are pure
//! domain types — no I/O, transport, or manager concerns live here.

// A2A is an EXPERIMENTAL, opt-in feature (excluded from `default` and `full`).
// Parts of the core domain — the agent registry, heartbeat watchdog, and several
// task-lifecycle helpers — are intentionally not yet wired into the serve path
// (that wiring was the de-scoped B4.5/B4.6 work), so they have no non-test
// caller. Allow dead_code module-wide until the feature is either completed or
// removed; do not drop this without re-wiring or pruning those items.
#![allow(dead_code)]

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
