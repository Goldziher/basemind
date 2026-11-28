//! The reads behind `code_refs`: call sites and the callers of one definition (`calls`, over the
//! name scan in `calls_scan`), implementations (`impls`), the definition a reference binds to
//! (`intel`), what an exact name match is (`matching`), how each call site was resolved
//! (`resolution`), a file's importers and the import cycles over the import graph (`graph`), and the
//! HTTP calls a route answers (`routes`, over the table in `route_table`). Each reads the index alone.

pub mod calls;
pub mod calls_scan;
pub mod graph;
pub mod impls;
pub mod intel;
pub mod matching;
pub mod resolution;
pub mod route_table;
pub mod routes;
