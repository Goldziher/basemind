//! A2A gRPC binding: tonic `A2AService` implementation backed by the core task domain.
//!
//! B2 implements and unit-tests the full `lf.a2a.v1.A2AService` (11 RPCs) here,
//! but no running transport mounts it yet — B3 stands up the axum server that
//! serves [`service::BasemindA2aService`] (and the JSON-RPC surface) over hyper. Until
//! then the service type and its methods are intentionally unreferenced from
//! non-test code; drop this module-level allow when B3 wires the service onto a
//! listener.
#![allow(dead_code)]

pub(crate) mod convert;
pub(crate) mod service;
