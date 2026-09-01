//! The load-bearing guarantee of the bounded read stack: **a cache miss changes latency, never an
//! answer.**
//!
//! The MCP read stack no longer holds every file's decoded outline for the process lifetime. It
//! holds a symbol-free file view plus a byte-charged LRU of outlines, and every whole-corpus tool
//! streams through that LRU. That is only a safe trade if the tool surface produces the SAME BYTES
//! whether the outline it needed was resident or had to be re-read from its blob.
//!
//! So this test runs the same corpus twice — once with `[resources] max_map_cache_mb = 0`
//! (unbounded: every outline stays resident, the pre-split behaviour) and once with `= 1`, a budget
//! the fixture deliberately overflows several times over — and asserts the responses are identical
//! modulo the timing field every response carries.
//!
//! The fixture is generated rather than hand-written for exactly that reason: a handful of small
//! files would sit inside a 1 MiB budget, both runs would be all-hits, and the test would pass
//! while proving nothing. `map_cache_budget_is_enforced_and_streams_the_whole_corpus` in
//! `src/mcp/state.rs` is the paired unit test that pins the eviction actually happening.

use std::path::Path;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Files in the generated corpus, and functions per file. Sized so the decoded outlines total
/// several MiB — comfortably past the 1 MiB budget the bounded arm runs under, so that arm really
/// does evict and re-read.
const FIXTURE_FILES: usize = 120;
const FNS_PER_FILE: usize = 150;

fn build_corpus() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    for f in 0..FIXTURE_FILES {
        let mut src = String::with_capacity(FNS_PER_FILE * 120);
        for i in 0..FNS_PER_FILE {
            // Long, distinct names and signatures: the outline's bulk is its owned strings, which
            // is what the byte charge measures.
            src.push_str(&format!(
                "/// WHY: keep the generated fixture outline realistically sized.\n\
                 pub fn module_{f:03}_operation_{i:03}_with_a_long_name(argument_one: u64, argument_two: &str) -> u64 {{\n\
                     let _ = argument_two;\n\
                     argument_one\n\
                 }}\n"
            ));
        }
        src.push_str(&format!(
            "pub struct Module{f:03}Widget {{ pub field_one: u64 }}\n\
             pub trait Module{f:03}Drawable {{ fn draw(&self); }}\n\
             impl Module{f:03}Drawable for Module{f:03}Widget {{ fn draw(&self) {{}} }}\n\
             pub fn module_{f:03}_caller() {{ module_{f:03}_operation_000_with_a_long_name(1, \"x\"); }}\n"
        ));
        std::fs::write(root.join(format!("module_{f:03}.rs")), src).unwrap();
    }
    dir
}

fn run_scan(root: &Path) {
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    // `#[tokio::test]`, so the scan runs on a dedicated std thread to mirror the production context.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
            )
            .expect("scan");
        });
    });
}

/// Write the budget into `<root>/.basemind/basemind.toml`.
///
/// The legacy in-cache config location, deliberately: `.basemind/` is excluded from every scan, so
/// switching budgets between the two runs cannot perturb the indexed corpus the runs are supposed
/// to be comparing.
fn write_budget(root: &Path, mb: usize) {
    let dir = root.join(".basemind");
    std::fs::create_dir_all(&dir).expect("create .basemind");
    std::fs::write(
        dir.join("basemind.toml"),
        format!("\"$schema\" = \"v1\"\n\n[resources]\nmax_map_cache_mb = {mb}\n"),
    )
    .expect("write config");
}

fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

fn text_of(result: &CallToolResult) -> String {
    use rmcp::model::ContentBlock;
    result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Drop the fields that legitimately differ between two runs of the same query: handler latency.
/// Everything else — totals, ordering, cursors, truncation flags, provenance — must match exactly.
fn scrub(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("elapsed_us");
            for v in map.values_mut() {
                scrub(v);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub),
        _ => {}
    }
}

/// Every tool call the comparison drives. Chosen to cover both consumer classes the split created:
/// point lookups (`outline`, `callers`), whole-corpus streams (`symbols`, `find`, `grep`,
/// `dependents`), and the graph lanes that project a streamed corpus into a memoized structure
/// (`neighbors`, `map`, `calls`).
///
/// `mode = "files"` is deliberately absent: it enumerates `store.index.files` directly rather than
/// the read stack, and that is an `AHashMap` whose iteration order is seeded per instance — so two
/// runs disagree regardless of cache budget. That is a real (pre-existing, separate) pagination
/// defect, not something this comparison can or should mask.
fn tool_matrix() -> Vec<(&'static str, Value)> {
    vec![
        ("code", json!({"mode": "outline", "path": "module_000.rs"})),
        ("code", json!({"mode": "outline", "path": "module_119.rs", "l2": true})),
        ("code", json!({"mode": "symbols", "name": "operation_001", "limit": 50})),
        ("code", json!({"mode": "symbols", "name": "caller", "limit": 200})),
        ("code", json!({"mode": "find", "query": "module_05", "limit": 50})),
        (
            "code",
            json!({"mode": "grep", "pattern": "module_00[0-3]_caller", "limit": 40}),
        ),
        ("code", json!({"mode": "dependents", "module": "Module007Drawable"})),
        (
            "code",
            json!({"mode": "callers", "name": "module_000_operation_000_with_a_long_name", "path": "module_000.rs"}),
        ),
        (
            "code",
            json!({"mode": "references", "name": "module_000_operation_000_with_a_long_name", "limit": 50}),
        ),
        (
            "code",
            json!({"mode": "implementations", "trait_name": "Module007Drawable"}),
        ),
        (
            "graph",
            json!({"mode": "neighbors", "name": "module_000_caller", "depth": 2}),
        ),
        ("graph", json!({"mode": "map", "max_nodes": 40})),
        (
            "graph",
            json!({"mode": "calls", "name": "module_000_caller", "direction": "callees"}),
        ),
    ]
}

async fn collect_responses(root: &Path) -> Vec<Value> {
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");
    let mut out = Vec::new();
    for (tool, args) in tool_matrix() {
        let result = service
            .call_tool(call_params(tool, args.clone()))
            .await
            .unwrap_or_else(|e| panic!("call {tool} {args}: {e}"));
        let mut parsed: Value = serde_json::from_str(&text_of(&result)).unwrap_or(Value::Null);
        scrub(&mut parsed);
        out.push(parsed);
    }
    service.cancel().await.ok();
    out
}

/// The whole point: identical bytes from a read stack that had to fault most of its outlines back
/// in, and one that never evicted a single entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_output_is_identical_bounded_and_unbounded() {
    let dir = build_corpus();
    let root = dir.path();
    run_scan(root);

    write_budget(root, 0);
    let unbounded = collect_responses(root).await;

    write_budget(root, 1);
    let bounded = collect_responses(root).await;

    assert_eq!(
        unbounded.len(),
        tool_matrix().len(),
        "every tool in the matrix must have answered"
    );
    for (i, (tool, args)) in tool_matrix().into_iter().enumerate() {
        if unbounded[i] != bounded[i] {
            // Rendered compactly and truncated: a whole-corpus response printed twice in full
            // buries the one line that actually differs.
            let left = serde_json::to_string(&unbounded[i]).unwrap_or_default();
            let right = serde_json::to_string(&bounded[i]).unwrap_or_default();
            let at = left
                .char_indices()
                .zip(right.char_indices())
                .find(|((_, a), (_, b))| a != b)
                .map_or(left.len().min(right.len()), |((i, _), _)| i);
            panic!(
                "response differs between an unbounded and a 1 MiB outline cache for {tool} {args}\n\
                 first divergence at byte {at}\n  unbounded: …{}\n  bounded:   …{}",
                &left[at.saturating_sub(80)..(at + 200).min(left.len())],
                &right[at.saturating_sub(80)..(at + 200).min(right.len())],
            );
        }
    }
    assert!(
        unbounded
            .iter()
            .any(|v| v.get("total").and_then(Value::as_u64).unwrap_or(0) > 0),
        "the fixture must actually produce hits, or the comparison is vacuous"
    );
}
