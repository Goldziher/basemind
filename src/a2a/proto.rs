//! Generated A2A protobuf + gRPC bindings.
//!
//! The two committed files under `generated/` are produced by `buf generate`
//! (driven by `buf.gen.yaml`): `lf.a2a.v1.rs` holds the prost message structs,
//! `lf.a2a.v1.tonic.rs` holds the tonic client/server. The prost file ends with
//! its own `include!("lf.a2a.v1.tonic.rs")`, so including the prost file alone
//! pulls both into this single `lf.a2a.v1` module (the tonic code reaches the
//! message types via `super::`).
//!
//! Hand-editing the generated files is forbidden — regenerate via `buf generate`.

/// The `lf.a2a.v1` protobuf package: message types plus the tonic client/server.
pub mod lf {
    pub mod a2a {
        pub mod v1 {
            include!("generated/lf.a2a.v1.rs");
        }
    }
}
