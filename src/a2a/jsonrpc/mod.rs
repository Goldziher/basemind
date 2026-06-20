//! A2A JSON-RPC 2.0 binding: the HTTP/SSE surface mirroring the gRPC service.
//!
//! The A2A JSON-RPC wire format is camelCase with a `kind` discriminator and
//! kebab-case task states — incompatible with basemind's snake_case core serde,
//! which also carries non-spec fields (`assignee`/`creator`/`deadline`). So this
//! module defines a dedicated DTO layer ([`dto`]) that maps core <-> wire, the
//! JSON-RPC envelope + A2A error codes ([`protocol`]), and the axum handlers
//! ([`handlers`]) that dispatch the 10 methods onto the shared
//! [`TaskFacade`](crate::a2a::core::task_facade::TaskFacade).
//!
//! Mounted on the shared axum listener by [`crate::a2a::server::build_router`]
//! ([`handlers::jsonrpc_handler`] + [`handlers::agent_card_handler`]).

pub(crate) mod convert;
pub(crate) mod dto;
pub(crate) mod handlers;
pub(crate) mod protocol;
