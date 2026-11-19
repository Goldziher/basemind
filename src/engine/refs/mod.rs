//! The reads behind `code_refs`: call sites and the callers of one definition (`calls`, over the
//! name scan in `calls_scan`), implementations (`impls`), the definition a reference binds to
//! (`intel`), what an exact name match is (`matching`), how each call site was resolved
//! (`resolution`), a file's importers and the import cycles over the import graph (`graph`), and the
//! HTTP calls a route answers (`routes`). Each reads the index alone.

pub(crate) mod calls;
pub(crate) mod calls_scan;
pub(crate) mod graph;
pub(crate) mod impls;
pub(crate) mod intel;
pub(crate) mod matching;
pub(crate) mod resolution;
pub(crate) mod routes;
