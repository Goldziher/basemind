//! `references` / `callers` / `implementations` must answer identically wherever the fjall index
//! lives. A writer session scans its own index; a daemon-backed session forwards the same scan to
//! the daemon; a session that can reach neither falls back to the in-RAM projection. On a generated
//! multi-file corpus all three must return byte-identical pages, cursors included, and the
//! forwarded routes must not build the projection at all (that is the memory bound).

#![cfg(all(test, feature = "comms", any(unix, windows)))]

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::helpers::RefsSource;
use super::index_route::IndexRoute;
use super::types::{FindCallersParams, FindReferencesParams};
use super::types_impls::FindImplementationsParams;
use super::{HostBackend, MapCache};
use crate::comms::index_read_proto::{IndexReadQuery, IndexReadResult};
use crate::config::ConfigV1;
use crate::path::RelPath;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

/// Files in the corpus. Three `get_arg*` call sites per file puts that name past the scan's own
/// 2,000-match cap, so the `total_is_partial` / cursor-resume path is covered, not just the easy one.
const FILES: usize = 720;

/// Write the corpus: every file declares a trait, implements one, and calls a handful of shared
/// names; a Python file adds a second language for the `implementations` language filter.
fn write_corpus(root: &Path) {
    for k in 0..FILES {
        let dir = root.join(format!("pkg{}", k % 6));
        std::fs::create_dir_all(&dir).expect("pkg dir");
        let source = format!(
            "pub trait Tr{t} {{ fn run(&self); }}\n\
             pub struct S{k};\n\
             impl Tr{t} for S{k} {{ fn run(&self) {{ get_arg(); shared_helper_{h}(); }} }}\n\
             pub fn driver_{k}() {{ get_arg(); get_arg_len(); other_{o}(); }}\n",
            t = k % 3,
            h = k % 5,
            o = k % 7,
        );
        std::fs::write(dir.join(format!("m{k}.rs")), source).expect("rust file");
    }
    std::fs::create_dir_all(root.join("py")).expect("py dir");
    std::fs::write(
        root.join("py/a.py"),
        b"class Base:\n    pass\n\n\nclass Child(Tr1):\n    def go(self):\n        get_arg()\n",
    )
    .expect("python file");
}

/// Stands in for the daemon's workspace pool: answers `host_index_read` from a writer store, after
/// a wire round trip so serde drift between the two ends fails here and not in production.
struct StoreHost {
    store: Mutex<Store>,
    /// When set, every read fails, as a daemon that is restarting would.
    broken: bool,
}

impl HostBackend for StoreHost {
    fn host_rescan(
        &self,
        _root: &Path,
        _paths: Option<Vec<std::path::PathBuf>>,
        _full: bool,
        _embed: bool,
    ) -> Result<crate::scanner::ScanStats, String> {
        Err("unused".into())
    }

    fn host_index_read(&self, _root: &Path, query: IndexReadQuery) -> Result<IndexReadResult, String> {
        if self.broken {
            return Err("daemon unavailable".into());
        }
        let query: IndexReadQuery = round_trip(&query);
        let store = self.store.lock().expect("store lock");
        let reply = super::index_read::index_read_against(&store, &query)?;
        Ok(round_trip(&reply))
    }

    fn host_resolved_refs(
        &self,
        _root: &Path,
        _query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> Result<crate::comms::resolved_proto::ResolvedRefResult, String> {
        Err("unused".into())
    }

    #[cfg(feature = "code-search")]
    fn host_code_search_lanes(
        &self,
        _root: &Path,
        _query: crate::comms::code_search_proto::CodeSearchLaneQuery,
    ) -> Result<crate::comms::code_search_proto::CodeSearchLaneResult, String> {
        Err("unused".into())
    }

    #[cfg(feature = "memory")]
    fn host_memory(
        &self,
        _root: &Path,
        _scope: &str,
        _op: crate::comms::memory_proto::MemoryOp,
    ) -> Result<crate::comms::memory_proto::MemoryOutcome, String> {
        Err("unused".into())
    }

    #[cfg(feature = "memory")]
    fn host_governance(
        &self,
        _root: &Path,
        _scope: &str,
        _op: crate::comms::proposals_proto::GovernanceOp,
    ) -> Result<crate::comms::proposals_proto::GovernanceOutcome, String> {
        Err("unused".into())
    }
}

fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
    rmp_serde::from_slice(&rmp_serde::to_vec_named(value).expect("encode")).expect("decode")
}

/// A scanned corpus plus the three routes over it and the read-only session's map.
struct Fixture {
    root: std::path::PathBuf,
    /// The writer store (owns the fjall lock).
    store: Arc<StoreHost>,
    local: IndexRoute,
    host: IndexRoute,
    broken_host: IndexRoute,
    /// The session's view: no fjall index, as on a `daemon_writer` front-end.
    session: Store,
    cache: MapCache,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    write_corpus(&root);
    let mut store = Store::open(&root, VIEW_WORKING).expect("open writer store");
    scan(
        &root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .expect("scan");
    let idx = store.index_db.clone().expect("writer opens the fjall index");
    let session = Store::open_read_only_no_index(&root, VIEW_WORKING).expect("index-less session store");
    assert!(session.index_db.is_none(), "the session must not hold the index");
    let cache = MapCache::build(&session, 0);
    let host: Arc<StoreHost> = Arc::new(StoreHost {
        store: Mutex::new(store),
        broken: false,
    });
    // A second store handle is not needed for the broken host: it never reads.
    let broken: Arc<dyn HostBackend> = Arc::new(StoreHost {
        store: Mutex::new(Store::open_read_only_no_index(&root, VIEW_WORKING).expect("store")),
        broken: true,
    });
    Fixture {
        local: IndexRoute::Local(idx),
        host: IndexRoute::Host {
            host: Arc::clone(&host) as Arc<dyn HostBackend>,
            root: root.clone(),
        },
        broken_host: IndexRoute::Host {
            host: broken,
            root: root.clone(),
        },
        store: host,
        root,
        session,
        cache,
        _dir: dir,
    }
}

/// The page a client sees, minus the only field that legitimately differs between runs.
fn decode(result: &rmcp::model::CallToolResult) -> Value {
    use rmcp::model::ContentBlock;
    let raw = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("text content");
    let mut value: Value = serde_json::from_str(&raw).expect("json");
    value.as_object_mut().expect("object").remove("elapsed_us");
    value
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

fn references_page(
    route: &IndexRoute,
    cache: &MapCache,
    name: &str,
    limit: u32,
    max_tokens: Option<u32>,
    cursor: Option<&str>,
) -> Value {
    decode(
        &block_on(super::helpers::run_find_references(
            route,
            FindReferencesParams {
                name: name.to_string(),
                limit: Some(limit),
                max_tokens,
                format: None,
                cursor: cursor.map(|c| super::cursor::Cursor(c.to_string())),
            },
            cache,
            || None,
            std::time::Instant::now(),
        ))
        .expect("references"),
    )
}

/// Follow `next_cursor` to the end, returning every page.
fn paginate(mut fetch: impl FnMut(Option<&str>) -> Value) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = fetch(cursor.as_deref());
        cursor = page.get("next_cursor").and_then(Value::as_str).map(str::to_string);
        pages.push(page);
        if cursor.is_none() {
            return pages;
        }
        assert!(pages.len() < 2_000, "pagination did not terminate");
    }
}

fn hit_count(pages: &[Value]) -> usize {
    pages.iter().map(|p| p["hits"].as_array().map_or(0, Vec::len)).sum()
}

/// Run `references` over every route and demand identical pages, cursors and totals.
fn assert_references_equivalent(fx: &Fixture, name: &str, limit: u32, max_tokens: Option<u32>) {
    let run =
        |route: &IndexRoute| paginate(|cursor| references_page(route, &fx.cache, name, limit, max_tokens, cursor));
    let local = run(&fx.local);
    assert!(
        hit_count(&local) > 0 || name == "no_such_name",
        "{name}: ground truth is non-empty"
    );
    assert_eq!(
        local,
        run(&fx.host),
        "{name}/{limit}: forwarded pages must equal the writer's"
    );
    assert_eq!(
        local,
        run(&IndexRoute::InRam),
        "{name}/{limit}: uncapped projection must equal the writer's"
    );
}

#[test]
fn references_are_identical_on_every_route_including_cursors() {
    let fx = fixture();
    let capped = references_page(&fx.local, &fx.cache, "get_arg", 250, None, None);
    assert_eq!(
        capped["total_is_partial"], true,
        "the corpus must really cross the scan cap, or the partial path goes untested: {capped}"
    );
    // Common name past the scan cap, rare names, a substring name, and an absent one.
    assert_references_equivalent(&fx, "get_arg", 250, None);
    assert_references_equivalent(&fx, "get_arg", 1000, None);
    assert_references_equivalent(&fx, "shared_helper_3", 7, None);
    assert_references_equivalent(&fx, "other_", 100, None);
    assert_references_equivalent(&fx, "other_3", 1, None);
    assert_references_equivalent(&fx, "no_such_name", 10, None);
    // The token budget re-anchors the cursor to the last KEPT hit; that must survive forwarding too.
    assert_references_equivalent(&fx, "shared_helper_3", 50, Some(300));
}

#[test]
fn a_forwarded_total_is_the_complete_count_not_a_capped_lower_bound() {
    let fx = fixture();
    // One `other_N()` call per file: the whole corpus, with no scan cap involved.
    let page = references_page(&fx.host, &fx.cache, "other_", 5, None, None);
    assert_eq!(
        page["total"], FILES as u64,
        "the total counts every file's call: {page}"
    );
    assert!(!page["total_is_partial"].as_bool().unwrap_or(false), "{page}");
    assert!(
        page["next_cursor"].is_string(),
        "more pages remain past the first five: {page}"
    );
}

#[test]
fn forwarded_routes_never_build_the_in_ram_projection() {
    let fx = fixture();
    let _ = references_page(&fx.host, &fx.cache, "get_arg", 100, None, None);
    let _ = decode(
        &block_on(super::helpers::run_find_implementations(
            &fx.host,
            FindImplementationsParams {
                trait_name: "Tr".to_string(),
                language: None,
                limit: Some(100),
                max_tokens: None,
                cursor: None,
            },
            &fx.cache,
            || None,
            std::time::Instant::now(),
        ))
        .expect("implementations"),
    );
    assert!(
        !fx.cache.projections_built(),
        "a daemon-backed session must answer from the forwarded scan and hold no O(corpus) projection"
    );
    assert!(!fx.cache.projections_capped());
}

#[test]
fn implementations_are_identical_on_every_route_including_language_filter_and_cursors() {
    let fx = fixture();
    for (trait_name, language, limit) in [
        ("Tr", None, 100u32),
        ("Tr1", None, 13),
        ("Tr", Some("rust"), 50),
        ("Tr1", Some("python"), 10),
        ("Tr", Some("python"), 10),
        ("NoSuchTrait", None, 10),
    ] {
        let run = |route: &IndexRoute| {
            paginate(|cursor| {
                decode(
                    &block_on(super::helpers::run_find_implementations(
                        route,
                        FindImplementationsParams {
                            trait_name: trait_name.to_string(),
                            language: language.map(str::to_string),
                            limit: Some(limit),
                            max_tokens: None,
                            cursor: cursor.map(|c| super::cursor::Cursor(c.to_string())),
                        },
                        &fx.cache,
                        || None,
                        std::time::Instant::now(),
                    ))
                    .expect("implementations"),
                )
            })
        };
        let local = run(&fx.local);
        assert_eq!(
            local,
            run(&fx.host),
            "{trait_name}/{language:?}: forwarded must equal writer"
        );
        assert_eq!(
            local,
            run(&IndexRoute::InRam),
            "{trait_name}/{language:?}: projection must equal writer"
        );
    }
    // Sanity: the corpus really does exercise a non-trivial answer, with positions filled in.
    let all = paginate(|cursor| {
        decode(
            &block_on(super::helpers::run_find_implementations(
                &fx.host,
                FindImplementationsParams {
                    trait_name: "Tr".to_string(),
                    language: None,
                    limit: Some(500),
                    max_tokens: None,
                    cursor: cursor.map(|c| super::cursor::Cursor(c.to_string())),
                },
                &fx.cache,
                || None,
                std::time::Instant::now(),
            ))
            .expect("implementations"),
        )
    });
    assert!(
        hit_count(&all) >= FILES,
        "at least one impl per rust file: {}",
        hit_count(&all)
    );
    assert!(
        all[0]["hits"][0]["start_row"].as_u64().unwrap_or(0) > 0,
        "the forwarded page carries the impl's position: {}",
        all[0]["hits"][0]
    );
}

#[test]
fn callers_scan_and_file_batches_are_identical_on_every_route() {
    let fx = fixture();
    let def = RelPath::from("pkg0/m0.rs".as_bytes());
    let run = |route: &IndexRoute| {
        paginate(|cursor| {
            decode(
                &block_on(super::helpers::run_find_callers(
                    &fx.session,
                    RefsSource::Local(&fx.session),
                    route,
                    &fx.root,
                    &fx.cache,
                    FindCallersParams {
                        path: def.clone(),
                        name: "driver_0".to_string(),
                        kind: None,
                        limit: Some(1),
                        max_tokens: None,
                        cursor: cursor.map(|c| super::cursor::Cursor(c.to_string())),
                    },
                    || None,
                    std::time::Instant::now(),
                ))
                .expect("callers"),
            )
        })
    };
    let local = run(&fx.local);
    assert_eq!(local, run(&fx.host));
    assert_eq!(local, run(&IndexRoute::InRam));

    // The per-file batch behind the resolved refinement, including a path the index has no calls for.
    let paths: Vec<RelPath> = ["pkg0/m0.rs", "pkg1/m1.rs", "pkg5/m5.rs", "py/a.py", "absent.rs"]
        .iter()
        .map(|p| RelPath::from(p.as_bytes()))
        .collect();
    let key = |calls: Vec<super::helpers_calls_scan::CallRef>| {
        calls
            .into_iter()
            .map(|c| (c.start_byte, c.callee, c.line, c.column))
            .collect::<Vec<_>>()
    };
    let local = block_on(fx.local.calls_in_files(&fx.cache, &paths)).expect("local batch");
    let host = block_on(fx.host.calls_in_files(&fx.cache, &paths)).expect("host batch");
    let in_ram = block_on(IndexRoute::InRam.calls_in_files(&fx.cache, &paths)).expect("in-ram batch");
    assert_eq!(local.len(), paths.len());
    assert!(local[0].len() >= 3, "m0.rs makes several calls");
    assert!(local[4].is_empty(), "an unindexed path has no calls");
    let normalise = |batch: Vec<Vec<_>>| {
        batch
            .into_iter()
            .map(|calls| {
                let mut calls = key(calls);
                calls.sort();
                calls
            })
            .collect::<Vec<_>>()
    };
    let local = normalise(local);
    assert_eq!(local, normalise(host));
    assert_eq!(local, normalise(in_ram));
}

#[test]
fn a_failed_forward_degrades_to_the_projection_instead_of_failing_the_tool() {
    let fx = fixture();
    let healthy = references_page(&fx.local, &fx.cache, "shared_helper_2", 50, None, None);
    let degraded = references_page(&fx.broken_host, &fx.cache, "shared_helper_2", 50, None, None);
    assert_eq!(healthy, degraded, "an uncapped fallback is still the complete answer");
    assert!(
        fx.cache.projections_built(),
        "the fallback is what builds the projection"
    );
}

#[test]
fn an_oversized_file_batch_is_refused_not_scanned() {
    let fx = fixture();
    let paths: Vec<RelPath> = (0..=crate::comms::index_read_proto::MAX_CALLS_IN_FILES)
        .map(|i| RelPath::from(format!("pkg0/m{i}.rs").as_bytes()))
        .collect();
    let store = fx.store.store.lock().expect("store");
    let error = super::index_read::index_read_against(&store, &IndexReadQuery::CallsInFiles { paths })
        .expect_err("an unbounded wire batch must be rejected");
    assert!(error.contains("at most"), "{error}");
}

fn corpus_paths() -> Vec<RelPath> {
    let mut paths: Vec<RelPath> = (0..FILES)
        .map(|k| RelPath::from(format!("pkg{}/m{k}.rs", k % 6).as_bytes()))
        .collect();
    paths.push(RelPath::from("py/a.py"));
    paths
}

#[test]
fn grep_bloom_verdicts_are_identical_on_the_local_and_forwarded_routes() {
    let fx = fixture();
    let paths = corpus_paths();
    let refs: Vec<&RelPath> = paths.iter().collect();
    for pattern in [
        "driver_7\\(",
        "shared_helper_3",
        "(?i)OTHER_5",
        "Child\\(Tr1\\)",
        "zzz_not_in_corpus",
    ] {
        let local = block_on(fx.local.grep_skip(&fx.root, pattern, &refs)).expect("local verdicts");
        let host = block_on(fx.host.grep_skip(&fx.root, pattern, &refs)).expect("forwarded verdicts");
        assert_eq!(local, host, "{pattern}: routes must agree");
        assert_eq!(local.len(), paths.len());
        let re = regex::Regex::new(pattern).expect("regex");
        for (rel, skipped) in paths.iter().zip(&local) {
            let text = std::fs::read_to_string(fx.root.join(rel.to_path_buf())).expect("read");
            assert!(
                !(*skipped && re.is_match(&text)),
                "{pattern}: {rel} matches but was skipped"
            );
        }
        assert!(local.iter().any(|&s| s), "{pattern}: a selective literal skips files");
    }
}

#[test]
fn a_pattern_without_a_required_literal_or_a_failed_forward_means_no_prefilter() {
    let fx = fixture();
    let paths = corpus_paths();
    let refs: Vec<&RelPath> = paths.iter().collect();
    assert!(block_on(fx.local.grep_skip(&fx.root, r"\w+", &refs)).is_none());
    assert!(block_on(fx.host.grep_skip(&fx.root, ".*", &refs)).is_none());
    assert!(block_on(fx.broken_host.grep_skip(&fx.root, "driver_7", &refs)).is_none());
}

#[test]
fn an_oversized_grep_bloom_batch_is_refused() {
    let fx = fixture();
    let paths: Vec<RelPath> = (0..=crate::comms::index_read_proto::MAX_GREP_BLOOM_PATHS)
        .map(|i| RelPath::from(format!("pkg0/m{i}.rs").as_bytes()))
        .collect();
    let store = fx.store.store.lock().expect("store");
    let error = super::index_read::index_read_against(
        &store,
        &IndexReadQuery::GrepBloom {
            pattern: "driver".into(),
            paths,
        },
    )
    .expect_err("an unbounded wire batch must be rejected");
    assert!(error.contains("at most"), "{error}");
}
