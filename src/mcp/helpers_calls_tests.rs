//! The reference and caller bodies against a scanned fixture, and how a resolved page merges.

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

/// The index a writer session reads: its own open fjall index.
fn local_index(store: &Store) -> crate::index::view::StandaloneView<crate::index::IndexDb> {
    crate::index::view::StandaloneView(store.index_db.clone().expect("a writer store opens the fjall index"))
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
                &local_index(&store),
                FindReferencesParams {
                    substring: false,
                    name: "target".to_string(),
                    limit: Some(500),
                    max_tokens: None,
                    format: None,
                    cursor: None,
                },
                || None,
                |_| {},
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
                &local_index(&store),
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
            ))
            .expect("find_callers"),
    );

    assert_eq!(
        callers.get("total").and_then(serde_json::Value::as_u64),
        Some(4),
        "find_callers must NOT shrink to the resolution-visible subset: {callers}"
    );
    let paths: Vec<&str> = callers
        .get("hits")
        .and_then(serde_json::Value::as_object)
        .expect("hits grouped by file")
        .keys()
        .map(String::as_str)
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
                &local_index(&store),
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
    let files = body
        .get("hits")
        .and_then(serde_json::Value::as_object)
        .expect("hits grouped by file");
    assert_eq!(
        files
            .get("consumer.ts")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(2),
        "the namespace-import callers resolution cannot see must still be reported: {body}"
    );
    assert!(
        files
            .values()
            .flat_map(|hits| hits.as_array().into_iter().flatten())
            .all(|h| h.get("resolved").is_some()),
        "every hit is annotated so the agent can tell proven from unproven: {body}"
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
                &local_index(&store),
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
    // Hits are listed under their file: `(path, hit)` for each.
    let hits: Vec<(&str, &serde_json::Value)> = body
        .get("hits")
        .and_then(serde_json::Value::as_object)
        .expect("hits grouped by file")
        .iter()
        .flat_map(|(path, hits)| {
            hits.as_array()
                .into_iter()
                .flatten()
                .map(move |hit| (path.as_str(), hit))
        })
        .collect();
    let proven: Vec<&str> = hits
        .iter()
        .filter(|(_, h)| h.get("resolved").and_then(serde_json::Value::as_bool) == Some(true))
        .map(|(path, _)| *path)
        .collect();
    assert_eq!(
        proven,
        vec!["util.ts", "util.ts"],
        "only the util.ts sites are proven — other.ts is never conflated INTO the proven set"
    );
    let unproven: Vec<&str> = hits
        .iter()
        .filter(|(_, h)| h.get("resolved").and_then(serde_json::Value::as_bool) == Some(false))
        .map(|(path, _)| *path)
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
        shared_line: false,
        stale: false,
    };
    let scan = super::CallScanPage::finish(0, false, Vec::new(), Vec::new(), Vec::new(), false, None);
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

/// A caller that renamed the definition at its import (`import { target as g }`) is one the name
/// scan cannot see; resolution adds it, keyed into the scan's order by its content key. That key
/// comes from the index's live keys, so an in-RAM map that lacks the file (still warming, or its
/// outline unreadable) no longer drops the caller.
#[cfg(feature = "code-intel-js")]
#[test]
fn a_renamed_import_caller_is_listed_when_the_in_ram_map_lacks_its_file() {
    use crate::mcp::types::FindCallersParams;
    use crate::path::RelPath;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("util.ts"), b"export function target() { return 1; }\n").expect("util.ts");
    std::fs::write(
        root.join("consumer.ts"),
        b"import { target as g } from './util';\ng();\n",
    )
    .expect("consumer.ts");
    let (store, cache) = scan_fixture(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let callers = |cache: &crate::mcp::MapCache| {
        decode(
            &runtime
                .block_on(super::run_find_callers(
                    &store,
                    &local_index(&store),
                    crate::mcp::tool_contract::IndexedFiles::contained(root),
                    cache,
                    FindCallersParams {
                        substring: false,
                        path: RelPath::from("util.ts".as_bytes()),
                        name: "target".to_string(),
                        kind: None,
                        line: None,
                        limit: Some(100),
                        max_tokens: None,
                        cursor: None,
                    },
                    || None,
                ))
                .expect("find_callers"),
        )
    };
    let renamed = |body: &serde_json::Value| {
        body.pointer("/hits/consumer.ts")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|hits| hits.iter().any(|hit| hit["callee"] == "g"))
    };

    let full = callers(&cache);
    assert!(renamed(&full), "resolution lists the renamed caller: {full}");
    let partial = callers(&crate::mcp::MapCache::empty());
    assert!(renamed(&partial), "a map without the file still lists it: {partial}");
    assert_eq!(partial["total"], full["total"]);
}

/// A `references` cursor is a position in the view's key order, and an edit moves the edited
/// file's hits within it: a page cut before the edit could skip or repeat them after it. The
/// cursor is refused as stale instead.
#[test]
fn a_references_cursor_cut_before_an_edit_is_refused_after_it() {
    use crate::mcp::types::FindReferencesParams;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("a.rs");
    std::fs::write(root.join("b.rs"), b"fn beta() { alpha(); }\n").expect("b.rs");
    std::fs::write(root.join("c.rs"), b"fn gamma() { alpha(); }\n").expect("c.rs");
    let (mut store, _) = scan_fixture(root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let page = |store: &Store, cursor: Option<super::Cursor>| {
        runtime.block_on(super::run_find_references(
            &local_index(store),
            FindReferencesParams {
                substring: false,
                name: "alpha".to_string(),
                limit: Some(1),
                max_tokens: None,
                format: None,
                cursor,
            },
            || None,
            |_| {},
        ))
    };
    let first = decode(&page(&store, None).expect("first page"));
    let cursor = super::Cursor(first["next_cursor"].as_str().expect("a second page").to_string());
    assert!(
        page(&store, Some(cursor.clone())).is_ok(),
        "an unchanged view continues"
    );

    std::fs::write(root.join("b.rs"), b"fn beta() { alpha(); alpha(); }\n").expect("edit");
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("rescan");

    let refused = page(&store, Some(cursor)).expect_err("a cursor cut before the edit");
    assert!(refused.message.contains("changed since this cursor"), "{refused:?}");
}

/// A session that holds no index (another process holds the lock, no daemon serves this one: a CLI
/// command beside a writer `serve`) refuses every `code_refs` relation that scans it, naming the fix
/// and marked busy so the CLI exits `3`, instead of answering from a partial in-RAM projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_without_an_index_is_refused_with_the_fix() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("a.rs");
    std::fs::write(root.join("b.rs"), b"fn beta() { alpha(); }\n").expect("b.rs");
    drop(scan_fixture(root));
    let store = Store::open_read_only_no_index(root, VIEW_WORKING).expect("index-less store");
    let telemetry_dir = store.basemind_dir.clone();
    let shared = crate::mcp::Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(ConfigV1::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).expect("git cache")),
        None,
        crate::mcp::ServerOptions {
            read_only: true,
            ..crate::mcp::ServerOptions::default()
        }
        .into(),
    );
    let state = crate::mcp::ServerState::for_connection(
        crate::mcp::ServedWorkspace::new(Arc::new(shared), &telemetry_dir),
        "test".into(),
    );
    let refs = |relation: Option<&str>, path: Option<&str>| crate::mcp::params::CodeRefsParams {
        name: "alpha".to_string(),
        relation: relation.map(str::to_string),
        path: path.map(crate::path::RelPath::from),
        line: None,
        column: None,
        match_mode: None,
        limit: None,
        cursor: None,
        max_tokens: None,
    };
    for params in [
        refs(None, None),
        refs(None, Some("a.rs")),
        refs(Some("implementations"), None),
    ] {
        let refused = crate::mcp::helpers_code::code_refs(&state, params)
            .await
            .expect_err("a session with no index cannot scan references");
        assert_eq!(refused.code, rmcp::model::ErrorCode::INVALID_REQUEST, "{refused:?}");
        assert!(refused.message.contains("holds this workspace's index"), "{refused:?}");
        #[cfg(all(feature = "comms", any(unix, windows)))]
        assert!(refused.message.contains("`basemind comms start`"), "{refused:?}");
        let exit = crate::cli::run_tool("code_refs", Err(refused)).expect_err("refused");
        assert_eq!(crate::cli::exit::exit_code_for(&exit), crate::cli::exit::BUSY);
    }
}

/// A daemon-hosted store whose index the daemon took back (a poisoned handle being reopened, or an
/// evicted workspace) is told so, not that no daemon serves it.
#[test]
fn a_hosted_store_without_its_index_is_told_the_daemon_released_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("a.rs");
    drop(scan_fixture(root));
    let mut store = Store::open_read_only_no_index(root, VIEW_WORKING).expect("index-less store");
    store.index_source = Some(std::sync::Arc::new(|| None));

    let Err(refused) = super::index_for_refs(&store) else {
        panic!("no index to lend");
    };
    assert!(refused.message.contains("the daemon released"), "{refused:?}");
    assert!(!refused.message.contains("no daemon serves it"), "{refused:?}");
    assert_eq!(
        refused.data.as_ref().and_then(|data| data.get(crate::mcp::BUSY_DATA)),
        Some(&serde_json::Value::Bool(true)),
        "busy, so the CLI exits 3"
    );
}
