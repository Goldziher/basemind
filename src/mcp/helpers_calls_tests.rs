//! The reference and caller bodies against a scanned fixture, and how a resolved page merges.

use super::super::helpers_calls_scan::scan_calls_in_ram;
use crate::config::ConfigV1;
use crate::scanner::{ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

/// Decode a `CallToolResult` back into JSON — the exact bytes an MCP client sees.
fn decode(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    use rmcp::model::ContentBlock;
    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    serde_json::from_str(&text).expect("tool response is JSON")
}

/// The route a writer session takes: its own open fjall index.
fn local_route(store: &Store) -> super::IndexRoute {
    super::IndexRoute::Local(Box::new(crate::index::view::StandaloneView(
        store.index_db.clone().expect("a writer store opens the fjall index"),
    )))
}

/// Scan `root` and return the store plus its map cache.
fn scan_fixture(root: &std::path::Path) -> (Store, crate::mcp::MapCache) {
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("scan");
    let cache = crate::mcp::MapCache::build(&store, 0);
    (store, cache)
}

/// **P0 regression pin.** `find_callers` MUST report the cross-file callers that scope
/// resolution cannot see, and MUST NOT present a resolution-limited subset as the complete
/// answer.
///
/// The fixture is the exact real-world shape that broke: Python's `from pkg import mod`
/// followed by `mod.target()` binds a *module*, not the function, so the cross-file join
/// (`src/intel/xfile.rs`) has no named export to bind and skips the importer entirely — every
/// such caller is invisible to resolution. Meanwhile the definition file's own self-call DOES
/// resolve intra-file (tree-sitter `locals`, no feature flag), so resolution "succeeds" with a
/// single hit. A resolution-first `find_callers` therefore returned `total: 1`, no truncation
/// flag, and an agent concluded almost nothing called `target`.
///
/// The name scan is the sound floor: `find_callers` must agree with `find_references` on the
/// count for an unambiguous name, and merely *annotate* which hits resolution could prove.
#[test]
fn find_callers_reports_cross_file_callers_resolution_cannot_see() {
    use crate::mcp::types::{FindCallersParams, FindReferencesParams};
    use crate::path::RelPath;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir(root.join("pkg")).expect("pkg dir");
    std::fs::write(root.join("pkg/__init__.py"), b"").expect("__init__.py");
    std::fs::write(
        root.join("pkg/mod.py"),
        b"def target():\n    return 1\n\n\ndef seed():\n    return target()\n",
    )
    .expect("mod.py");
    std::fs::write(
        root.join("caller_a.py"),
        b"from pkg import mod\n\n\ndef go():\n    return mod.target()\n",
    )
    .expect("caller_a.py");
    std::fs::write(
        root.join("caller_b.py"),
        b"from pkg import mod\n\n\ndef go2():\n    return mod.target() + mod.target()\n",
    )
    .expect("caller_b.py");

    let (store, cache) = scan_fixture(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let references = decode(
        &runtime
            .block_on(super::run_find_references(
                &local_route(&store),
                FindReferencesParams {
                    substring: false,
                    name: "target".to_string(),
                    limit: Some(500),
                    max_tokens: None,
                    format: None,
                    cursor: None,
                },
                &cache,
                || None,
                std::time::Instant::now(),
            ))
            .expect("find_references"),
    );
    assert_eq!(
        references.get("total").and_then(serde_json::Value::as_u64),
        Some(4),
        "ground truth: find_references sees all 4 target() call sites: {references}"
    );

    let callers = decode(
        &runtime
            .block_on(super::run_find_callers(
                &store,
                super::RefsSource::Local(&store),
                &local_route(&store),
                crate::mcp::tool_contract::IndexedFiles::contained(root),
                &cache,
                FindCallersParams {
                    substring: false,
                    path: RelPath::from("pkg/mod.py".as_bytes()),
                    name: "target".to_string(),
                    kind: None,
                    line: None,
                    limit: Some(500),
                    max_tokens: None,
                    cursor: None,
                },
                || None,
                std::time::Instant::now(),
            ))
            .expect("find_callers"),
    );

    assert_eq!(
        callers.get("total").and_then(serde_json::Value::as_u64),
        Some(4),
        "find_callers must NOT shrink to the resolution-visible subset: {callers}"
    );
    let hits = callers.get("hits").and_then(serde_json::Value::as_array).expect("hits");
    let paths: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("path").and_then(serde_json::Value::as_str))
        .collect();
    assert!(
        paths.contains(&"caller_a.py") && paths.contains(&"caller_b.py"),
        "the module-import callers resolution cannot see must still be reported: {paths:?}"
    );
    assert_eq!(
        callers.get("total").and_then(serde_json::Value::as_u64),
        references.get("total").and_then(serde_json::Value::as_u64),
        "find_callers and find_references must agree on an unambiguous name"
    );

    let resolved_total = callers
        .get("resolved_total")
        .and_then(serde_json::Value::as_u64)
        .expect("resolved_total is always reported");
    assert!(
        resolved_total <= 4,
        "resolved_total is a lower bound on the truth, never above total: {callers}"
    );
}

/// **P0 regression pin, JS/TS engine.** The reported production case: a TS hook whose
/// `find_callers` returned `total: 2` (both in its own file) while `find_references` returned
/// 172 across 159 files.
///
/// Reproduced exactly: `util.ts` calls `target()` itself, so oxc resolves that intra edge and
/// resolution "succeeds". `consumer.ts` reaches it through a NAMESPACE import
/// (`import * as util`), which binds a module object, not `target` — `intel::xfile` finds no
/// named export to bind and skips the importer, so its two call sites are invisible to
/// resolution. The old resolution-first code therefore returned `total: 1`, no truncation flag,
/// and an agent concluded nothing external called the hook.
#[cfg(feature = "code-intel-js")]
#[test]
fn find_callers_reports_namespace_import_callers_the_js_resolver_cannot_bind() {
    use crate::mcp::types::FindCallersParams;
    use crate::path::RelPath;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("util.ts"),
        b"export function target() { return 1; }\ntarget();\n",
    )
    .expect("util.ts");
    std::fs::write(
        root.join("consumer.ts"),
        b"import * as util from './util';\nutil.target();\nutil.target();\n",
    )
    .expect("consumer.ts");
    let (store, cache) = scan_fixture(root);

    let body = decode(
        &tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(super::run_find_callers(
                &store,
                super::RefsSource::Local(&store),
                &local_route(&store),
                crate::mcp::tool_contract::IndexedFiles::contained(root),
                &cache,
                FindCallersParams {
                    substring: false,
                    path: RelPath::from("util.ts".as_bytes()),
                    name: "target".to_string(),
                    kind: None,
                    line: None,
                    limit: Some(500),
                    max_tokens: None,
                    cursor: None,
                },
                || None,
                std::time::Instant::now(),
            ))
            .expect("find_callers"),
    );

    assert_eq!(
        body.get("total").and_then(serde_json::Value::as_u64),
        Some(3),
        "all 3 call sites — the resolvable self-call AND the 2 namespace-import callers: {body}"
    );
    assert_eq!(
        body.get("resolved_total").and_then(serde_json::Value::as_u64),
        Some(1),
        "resolution can only PROVE the intra-file self-call — it must not pass that off as the total"
    );
    let hits = body.get("hits").and_then(serde_json::Value::as_array).expect("hits");
    let consumer_hits = hits
        .iter()
        .filter(|h| h.get("path").and_then(serde_json::Value::as_str) == Some("consumer.ts"))
        .count();
    assert_eq!(
        consumer_hits, 2,
        "the namespace-import callers resolution cannot see must still be reported: {body}"
    );
    assert!(
        hits.iter().all(|h| h.get("resolved").is_some()),
        "every hit is annotated so the agent can tell proven from unproven: {body}"
    );
}

/// The in-RAM index (built from blobs, used by read-only sessions) must return
/// the same references the Fjall path would — this is what keeps `find_references`
/// working for the 2nd+ concurrent session that can't open the Fjall lock.
#[test]
fn in_ram_call_index_resolves_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("a.rs");
    std::fs::write(root.join("b.rs"), b"fn beta() { alpha(); alpha(); }\n").expect("b.rs");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("scan");

    let cache = crate::mcp::MapCache::build(&store, 0);
    let page = scan_calls_in_ram(cache.calls_projection(), "alpha", super::Matching::Substring, 100, None);
    assert_eq!(page.total, 2, "two alpha() call sites in b.rs");
    assert_eq!(page.hits.len(), 2);
    assert!(page.hits.iter().all(|h| h.callee == "alpha"));
    assert!(
        page.hits.iter().all(|h| h.path.as_str() == Some("b.rs")),
        "both references live in b.rs"
    );
}

/// Precision is preserved, but as an ANNOTATION rather than a silent filter: resolution proves
/// only the callers that bind to *this* definition (never a same-named function in another
/// file), yet the same-named site is still REPORTED — flagged `resolved: false` — instead of
/// being dropped from the answer. Dropping is what made `find_callers` under-report; a caller
/// that wants precision filters on the flag.
///
/// Also pins the offset-alignment finding: the L1 node `start_byte` differs from the resolver's
/// `def_start` identifier byte, so the proven set can only be built by recovering the true
/// `def_start` from the blob. Feature-gated — only oxc (JS/TS) resolves top-level function calls
/// to their definition today.
#[cfg(feature = "code-intel-js")]
#[test]
fn find_callers_annotates_resolved_edges_without_dropping_same_name_sites() {
    use crate::mcp::types::FindCallersParams;
    use crate::path::RelPath;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("util.ts"),
        b"export function target() { return 1; }\ntarget();\ntarget();\n",
    )
    .expect("util.ts");
    std::fs::write(root.join("other.ts"), b"function target() { return 3; }\ntarget();\n").expect("other.ts");
    let (store, cache) = scan_fixture(root);

    let def_path = RelPath::from("util.ts".as_bytes());
    let l1 = crate::query::file_outline(&store, &def_path).expect("outline");
    let sym = l1
        .symbols
        .iter()
        .find(|s| s.name == "target" && s.kind == crate::extract::SymbolKind::Function)
        .cloned()
        .expect("util.ts target function symbol");

    let entry = store.lookup(&def_path).expect("indexed");
    let refs = store
        .read_resolved_by_hex(&entry.hash_hex)
        .expect("read blob")
        .expect("resolution facts present");
    assert!(
        !refs.intra.iter().any(|e| e.def_start == sym.start_byte),
        "L1 node start_byte must differ from the resolver's def identifier byte"
    );

    let body = decode(
        &tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(super::run_find_callers(
                &store,
                super::RefsSource::Local(&store),
                &local_route(&store),
                crate::mcp::tool_contract::IndexedFiles::contained(root),
                &cache,
                FindCallersParams {
                    substring: false,
                    path: def_path.clone(),
                    name: "target".to_string(),
                    kind: None,
                    line: None,
                    limit: Some(100),
                    max_tokens: None,
                    cursor: None,
                },
                || None,
                std::time::Instant::now(),
            ))
            .expect("find_callers"),
    );

    assert_eq!(
        body.get("resolved_total").and_then(serde_json::Value::as_u64),
        Some(2),
        "exactly the two util.ts callers are PROVEN to bind to util.ts target: {body}"
    );
    assert_eq!(
        body.get("total").and_then(serde_json::Value::as_u64),
        Some(3),
        "the other.ts same-name call site is still reported, not dropped: {body}"
    );
    let hits = body.get("hits").and_then(serde_json::Value::as_array).expect("hits");
    let proven: Vec<&str> = hits
        .iter()
        .filter(|h| h.get("resolved").and_then(serde_json::Value::as_bool) == Some(true))
        .filter_map(|h| h.get("path").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        proven,
        vec!["util.ts", "util.ts"],
        "only the util.ts sites are proven — other.ts is never conflated INTO the proven set"
    );
    let unproven: Vec<&str> = hits
        .iter()
        .filter(|h| h.get("resolved").and_then(serde_json::Value::as_bool) == Some(false))
        .filter_map(|h| h.get("path").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        unproven,
        vec!["other.ts"],
        "the same-name site is surfaced as unproven, so a precision-seeking caller can filter it"
    );
}

/// A scan that stopped at its examined-keys cap has seen nothing past the key it stopped
/// after. An aliased hit beyond that key waits for the page that gets there, and the cursor
/// stays on the stop key so that page continues the scan.
#[test]
fn merging_aliased_hits_keeps_a_capped_scan_s_resume_point() {
    use super::{ResolvedCallers, merge_resolved};
    use crate::mcp::types::ReferenceHit;

    let hit = || ReferenceHit {
        path: crate::path::RelPath::from("a.rs"),
        line: 1,
        column: 0,
        callee: "g".to_string(),
        resolved: Some(true),
    };
    let scan = scan_calls_in_ram(
        &super::super::helpers_calls_scan::InRamCallIndex::empty(),
        "f",
        super::Matching::Exact,
        10,
        None,
    );
    let page = super::CallScanPage {
        total_is_partial: true,
        next_cursor: Some(super::Cursor::encode_fjall(&[5])),
        stopped_after: Some(vec![5]),
        ..scan
    };
    let resolved = ResolvedCallers {
        aliased: vec![(vec![3], hit()), (vec![9], hit())],
        ..ResolvedCallers::default()
    };
    let merged = merge_resolved(page, Some(resolved), 10, None);
    assert_eq!(
        merged.hit_keys,
        [vec![3]],
        "the hit past the stop key is not emitted yet"
    );
    assert_eq!(merged.stopped_after, Some(vec![5]));
    assert_eq!(
        merged.next_cursor.map(|cursor| cursor.0),
        Some(super::Cursor::encode_fjall(&[5]).0)
    );
}
