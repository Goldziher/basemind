//! A downstream program that depends on `basemind-engine` and nothing else of basemind.
//!
//! The crate carries no code: `tests/embed.rs` is the embedder. It stays outside the published set
//! (`publish = false`) so CI can build it on every platform without a second `basemind` crate in the
//! graph, which would hide a dependency of the engine on the CLI or MCP layer.
