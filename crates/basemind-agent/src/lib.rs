//! `basemind-agent` — a model-agnostic coding-agent engine built on the basemind code map.
//!
//! The engine is UI-agnostic: a front-end (ratatui, tauri, ...) drives it entirely through the
//! [`transport::AgentClient`] boundary — sending [`command::AgentCommand`]s and consuming
//! [`event::AgentEvent`]s. Providers are reached through liter-llm behind the small
//! [`model::ModelClient`] trait, so the turn-loop is model-agnostic and the tests are network-free.

pub mod command;
pub mod config;
pub mod error;
pub mod event;
pub mod history;
pub mod model;
pub mod permission;
pub mod provider;
pub mod session;
pub mod tools;
pub mod transport;

pub use command::{AgentCommand, PermissionDecision};
pub use config::{AgentConfig, Role, RoleModels};
pub use error::{AgentError, Result};
pub use event::{AgentEvent, StopReason};
pub use history::{History, SessionMeta, SessionStore};
pub use model::{LiterModelClient, ModelClient};
pub use permission::{Decision, PermissionClaim, PermissionEngine};
pub use provider::{ProviderPool, ResolvedRole};
pub use session::{Session, StreamAssembler, TurnContext, run_turn};
pub use tools::{ToolCtx, ToolRegistry};
pub use transport::{AgentClient, EngineEndpoint, InProcAgentClient, in_proc_channel};
