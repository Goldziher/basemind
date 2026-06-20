//! A2A (Agent-to-Agent) protocol bindings.
//!
//! Phase B0a foundation: the official A2A gRPC service (`lf.a2a.v1.A2AService`,
//! 11 RPCs) and its message types, generated from the vendored proto under
//! `proto/a2a/v1/a2a.proto`. Phase B2 adds the `grpc` binding — a tonic
//! `A2AService` implementation ([`grpc::service::BasemindA2aService`]) backed by the
//! [`core`] task domain through the shared [`state::A2aState`]. Not yet mounted
//! on a running transport; B3 stands up the axum server.

pub(crate) mod core;
pub(crate) mod grpc;
pub mod proto;
pub(crate) mod state;

/// Crate-internal handle on the generated `lf.a2a.v1` package (prost message structs plus the
/// tonic `a2a_service_client` / `a2a_service_server` modules). Kept `pub(crate)` until a later
/// phase needs to expose the full generated surface; external callers use the flat aliases below.
pub(crate) use proto::lf::a2a::v1;

// Flat aliases for the most commonly reached surface.
pub use v1::a2a_service_client::A2aServiceClient;
pub use v1::a2a_service_server::{A2aService, A2aServiceServer};
