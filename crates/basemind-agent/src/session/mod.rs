//! Session orchestration: the turn-loop and its streaming support.
//!
//! [`run_turn`] drives one user turn to completion — streaming the model, assembling tool calls,
//! permission-gating and executing them, and feeding results back — emitting [`crate::event::AgentEvent`]s
//! and consuming [`crate::command::AgentCommand`]s (permission replies / cancel) along the way.

pub mod runner;
pub mod stream_assembler;
pub mod turn;

pub use runner::Session;
pub use stream_assembler::{AssembledTurn, StreamAssembler};
pub use turn::{TurnContext, run_turn};
