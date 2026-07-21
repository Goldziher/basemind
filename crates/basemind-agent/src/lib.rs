//! `basemind-agent` — a model-agnostic coding-agent engine built on the basemind code map.
//!
//! The engine is UI-agnostic: a front-end (ratatui, tauri, ...) drives it entirely through the
//! [`transport::AgentClient`] boundary — sending [`command::AgentCommand`]s and consuming
//! [`event::AgentEvent`]s. Providers are reached through liter-llm behind the small
//! [`model::ModelClient`] trait, so the turn-loop is model-agnostic and the tests are network-free.

pub mod command;
pub mod error;
pub mod event;
pub mod model;
pub mod permission;
pub mod session;
pub mod transport;

pub use command::{AgentCommand, PermissionDecision};
pub use error::{AgentError, Result};
pub use event::{AgentEvent, StopReason};
pub use model::{LiterModelClient, ModelClient};
pub use transport::{AgentClient, EngineEndpoint, InProcAgentClient, in_proc_channel};
