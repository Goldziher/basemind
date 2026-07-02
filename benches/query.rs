//! Query read-path microbenchmarks.
//!
//! Builds a small synthetic repo in a tempdir, scans it once via the public
//! `basemind::scanner::scan` into a `basemind::store::Store`, then benches the
//! three read-side helpers agents hit most: `search_symbols`, `file_outline`,
//! and `dependents_of`. The store is built once in `setup` so the bench measures
//! the query path, not the scan.

use basemind::config::ConfigV1;
use basemind::query::{dependents_of, file_outline, search_symbols};
use basemind::scanner::{ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tempfile::TempDir;

const FILE_COUNT: usize = 40;

/// Generate a deterministic Rust module referencing a shared `common` module so
/// `dependents_of("common")` has real hits and `search_symbols` has many matches.
fn module_source(index: usize) -> String {
    format!(
        "use crate::common::Shared;\n\
         use std::collections::HashMap;\n\
         \n\
         pub struct Widget{index} {{ id: u64, cache: HashMap<u64, Shared> }}\n\
         \n\
         impl Widget{index} {{\n\
         \tpub fn new() -> Self {{ Self {{ id: {index}, cache: HashMap::new() }} }}\n\
         \tpub fn process_widget(&self, input: &Shared) -> u64 {{ self.id + input.weight() }}\n\
         \tpub fn reset_widget(&mut self) {{ self.cache.clear(); }}\n\
         }}\n\
         \n\
         pub fn build_widget_{index}() -> Widget{index} {{ Widget{index}::new() }}\n"
    )
}

/// Scan a freshly-populated tempdir and return the open store. The `TempDir` is
/// returned alongside so it outlives the store (drop order keeps blobs on disk).
fn setup() -> (TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    std::fs::write(
        root.join("common.rs"),
        b"pub struct Shared { pub w: u64 }\nimpl Shared { pub fn weight(&self) -> u64 { self.w } }\n",
    )
    .expect("write common.rs");
    for i in 0..FILE_COUNT {
        std::fs::write(root.join(format!("widget_{i}.rs")), module_source(i)).expect("write module");
    }

    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let cfg = ConfigV1::with_defaults();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree).expect("scan");
    (dir, store)
}

fn bench_query(c: &mut Criterion) {
    let (_dir, store) = setup();

    let mut group = c.benchmark_group("query");

    group.bench_function("search_symbols/process_widget", |b| {
        b.iter(|| search_symbols(&store, black_box("process_widget"), None).unwrap());
    });
    group.bench_function("file_outline/widget_0", |b| {
        b.iter(|| file_outline(&store, black_box("widget_0.rs")).unwrap());
    });
    group.bench_function("dependents_of/common", |b| {
        b.iter(|| dependents_of(&store, black_box("common")).unwrap());
    });

    group.finish();
    // `_dir` is held to the end of the fn so the on-disk blobs outlive every bench.
    drop(_dir);
}

criterion_group!(benches, bench_query);
criterion_main!(benches);
