//! On-demand web crawl tier: crawlberg → markdown → xberg chunk + embed → LanceDB.
//!
//! Gated on `feature = "crawl"`. Lives under its own module so the network
//! surface area (crawlberg, robots handling, scope tagging) stays inspectable
//! in one place. Mirrors the layout of `scanner_docs.rs` for the scanner doc
//! tier — `engine.rs` builds the shared crawler handle, `ingest.rs` is the
//! shared chunk + embed + LanceDB write path used by `web_scrape` / `web_crawl`.

#![cfg(feature = "crawl")]

pub mod engine;
pub mod ingest;

pub use engine::build_engine;
pub use ingest::{IndexedPage, index_page};
