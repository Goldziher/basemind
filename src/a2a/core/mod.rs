//! A2A core domain types.
//!
//! Foundation types for the A2A (Agent-to-Agent) task system: identity
//! newtypes and agent metadata ([`types`]) plus the task state machine,
//! messages, artifacts, and task metadata ([`task_types`]). These are pure
//! domain types — no I/O, transport, or manager concerns live here.

// Wave-1 foundation: these domain types are ported ahead of the managers,
// registry, and transport that consume them in later A2A phases, so they have
// no non-test callers yet. Allow dead_code module-wide until those waves land.
#![allow(dead_code)]

pub mod bus;
pub mod push_notifications;
pub mod registry;
pub mod router;
pub mod task_manager;
pub mod task_types;
pub mod types;
