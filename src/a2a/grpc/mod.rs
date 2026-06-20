//! A2A gRPC binding: tonic `A2AService` implementation backed by the core task domain.
//!
//! Implements the full `lf.a2a.v1.A2AService` (11 RPCs). [`service::BasemindA2aService`]
//! is mounted by [`crate::a2a::server::build_router`] onto the shared axum listener
//! (HTTP/2 h2c), alongside the JSON-RPC surface.

pub(crate) mod convert;
pub(crate) mod service;
