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
//! Like [`crate::a2a::grpc`], this surface is implemented and unit-tested but
//! only mounted on a running listener by [`crate::a2a::server`]; drop the
//! module-level allow once `basemind serve` starts the A2A server.
#![allow(dead_code)]

pub(crate) mod convert;
pub(crate) mod dto;
pub(crate) mod handlers;
pub(crate) mod protocol;
