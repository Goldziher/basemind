//! Session orchestration: the turn-loop and its streaming support.
//!
//! For now this holds the [`stream_assembler`], which reassembles streamed provider chunks into a
//! finished assistant turn. The turn-loop itself lands in a later slice once the tool registry and
//! permission engine are in place.

pub mod stream_assembler;

pub use stream_assembler::{AssembledTurn, StreamAssembler};
