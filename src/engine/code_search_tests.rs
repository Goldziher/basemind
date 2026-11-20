//! Hit hydration of `code_search`: a hit cites the paths that hold its content now, and each
//! sidecar is decoded once per query.

use std::path::Path;

use super::*;
use crate::config::ConfigV1;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::search::bm25::bm25_search;

const BODY: &str = "/// Retry a failed upload with exponential backoff.\npub fn retry_upload(attempts: u32) -> u32 {\n    \
                    attempts * 2\n}\n";

fn scanned(root: &Path) -> Store {
    crate::store::init_isolated_cache();
    let cfg = ConfigV1::with_defaults();
    let mut store = Store::open(root, crate::store::VIEW_WORKING).expect("open");
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        crate::retrieval_wiring::backend(),
    )
    .expect("scan");
    store
}

const UNFILTERED: Filters<'static> = Filters {
    tool: "code_search",
    path: None,
    language: None,
};

fn keyword_seeds(store: &Store, query: &str) -> Vec<CodeSearchHit> {
    let db = store.index_db.as_ref().expect("index db");
    bm25_search(&db.view(), query, 50)
        .into_iter()
        .map(|hit| seed(hit.chunk_id))
        .collect()
}

#[test]
fn a_renamed_file_is_cited_at_its_new_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("old.rs"), BODY).unwrap();
    drop(scanned(dir.path()));
    std::fs::rename(dir.path().join("old.rs"), dir.path().join("new.rs")).unwrap();
    let store = scanned(dir.path());

    let hydrated = hydrate_hits(&store, keyword_seeds(&store, "retry upload"), 10, 0, &UNFILTERED);

    assert!(!hydrated.hits.is_empty(), "the keyword lane finds the chunk");
    for hit in &hydrated.hits {
        assert_eq!(hit.path, "new.rs", "{hit:?}");
        assert!(hit.also_at.is_empty(), "{hit:?}");
    }
    assert_eq!(hydrated.omitted, 0);
}

#[test]
fn identical_files_are_one_hit_listing_every_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    for name in ["b.rs", "a.rs", "vendor_copy.rs"] {
        std::fs::write(dir.path().join(name), BODY).unwrap();
    }
    let store = scanned(dir.path());

    let seeds = keyword_seeds(&store, "retry upload");
    let distinct = seeds.len();
    // A vector lane holds one row per path, so the same id can be ranked more than once.
    let doubled: Vec<CodeSearchHit> = seeds.iter().flat_map(|s| [s.clone(), s.clone()]).collect();
    let hydrated = hydrate_hits(&store, doubled, 10, 10, &UNFILTERED);

    assert_eq!(hydrated.hits.len(), distinct, "one hit per chunk, not per copy");
    let top = &hydrated.hits[0];
    assert_eq!(top.path, "a.rs", "the first sorted path is cited");
    assert_eq!(top.also_at, ["b.rs", "vendor_copy.rs"]);
    assert_eq!(
        hydrated.texts.len(),
        hydrated.hits.len(),
        "rerank bodies line up with hits"
    );
}

/// A hit cites only the copies the filters pass, and a chunk no passing file holds is dropped
/// without counting as stale.
#[test]
fn a_filtered_search_cites_the_copies_the_filters_pass() {
    let dir = tempfile::tempdir().expect("tempdir");
    for name in ["src/net/a.rs", "src/net_b.rs", "lib/src/net/c.rs"] {
        let file = dir.path().join(name);
        std::fs::create_dir_all(file.parent().expect("parent")).unwrap();
        std::fs::write(file, BODY).unwrap();
    }
    let store = scanned(dir.path());
    let hydrate = |path: &str, language: Option<&str>| {
        let path = crate::path_filter::PathFilter::parse(path).expect("filter");
        let filters = Filters {
            tool: "code_search",
            path: Some(&path),
            language,
        };
        let hydrated = hydrate_hits(&store, keyword_seeds(&store, "retry upload"), 10, 0, &filters);
        assert_eq!(hydrated.omitted, 0);
        hydrated
            .hits
            .iter()
            .map(|hit| {
                let mut cited = vec![hit.path.clone()];
                cited.extend(hit.also_at.iter().cloned());
                cited
            })
            .collect::<Vec<_>>()
    };

    let cites = |path: &str, language: Option<&str>, expected: &[&str]| {
        let hits = hydrate(path, language);
        assert!(!hits.is_empty(), "{path}: the filter keeps the chunk");
        for cited in hits {
            assert_eq!(cited, expected, "{path}");
        }
    };
    cites("", None, &["lib/src/net/c.rs", "src/net/a.rs", "src/net_b.rs"]);
    cites("src/net/", None, &["src/net/a.rs"]);
    cites("src/net", None, &["src/net/a.rs", "src/net_b.rs"]);
    cites("**/net/*.rs", Some("rust"), &["lib/src/net/c.rs", "src/net/a.rs"]);
    assert!(
        hydrate("net/", None).is_empty(),
        "a fragment inside the paths is no prefix of them"
    );
    assert!(hydrate("src/", Some("python")).is_empty(), "no copy is python");
}

#[test]
fn a_chunk_no_file_holds_is_dropped_and_counted() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.rs"), BODY).unwrap();
    let store = scanned(dir.path());
    let mut seeds = vec![seed("00ff:0".to_string())];
    seeds.extend(keyword_seeds(&store, "retry upload"));

    let hydrated = hydrate_hits(&store, seeds, 10, 0, &UNFILTERED);

    assert_eq!(hydrated.omitted, 1);
    assert!(hydrated.hits.iter().all(|h| h.path == "a.rs"));
}

#[test]
fn each_sidecar_is_decoded_once_per_query() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body: String = (0..12)
        .map(|i| format!("/// Retry step {i}.\npub fn retry_step_{i}(n: u32) -> u32 {{ n + {i} }}\n\n"))
        .collect();
    std::fs::write(dir.path().join("steps.rs"), body).unwrap();
    let store = scanned(dir.path());
    let seeds = keyword_seeds(&store, "retry");
    assert!(seeds.len() > 1, "several chunks of one file rank: {}", seeds.len());

    let mut hydrator = ChunkHydrator::new(&store);
    hydrator.resolve_paths(seeds.iter().map(|s| s.chunk_id.as_str()));
    for hit in &seeds {
        assert!(hydrator.chunk(&hit.chunk_id).is_some());
    }

    assert_eq!(hydrator.decodes(), 1, "{} chunks, one sidecar", seeds.len());
}

#[test]
fn a_prose_query_reaches_camel_case_identifiers() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("refs.ts"),
        "export function findReferences(name: string): number {\n  return name.length;\n}\n",
    )
    .unwrap();
    let store = scanned(dir.path());

    let hydrated = hydrate_hits(&store, keyword_seeds(&store, "find references"), 10, 0, &UNFILTERED);

    assert!(
        hydrated
            .hits
            .iter()
            .any(|h| h.path == "refs.ts" && h.symbol == "findReferences"),
        "{:?}",
        hydrated.hits
    );
}

#[test]
fn common_terms_are_skipped_and_candidates_are_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..12 {
        let rare = if i < 2 { " zebraword" } else { "" };
        std::fs::write(
            dir.path().join(format!("m{i}.rs")),
            format!("/// common helper{rare}\npub fn helper_{i}() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    let store = scanned(dir.path());
    let db = store.index_db.as_ref().expect("index db");
    let view = db.view();

    let rare = bm25_search(&view, "zebraword", 50).len();
    assert!(rare > 0);
    let (hits, work) = crate::search::bm25::bm25_search_counted(&view, "common zebraword", 50, 20_000);
    assert_eq!(work.terms_skipped, 1, "`common` is in every chunk: {work:?}");
    assert_eq!(
        work.candidates, rare,
        "only the rare term's chunks are scored: {work:?}"
    );
    assert_eq!(hits.len(), rare);

    let (hits, work) = crate::search::bm25::bm25_search_counted(&view, "common", 10, 3);
    assert_eq!(
        work.terms_skipped, 0,
        "a query of common terms only still runs: {work:?}"
    );
    assert_eq!(work.candidates, 3, "the accumulator stops at the bound: {work:?}");
    assert_eq!(hits.len(), 3);
}

/// A session that holds no index and is not hosted by the daemon (another process holds the lock,
/// no daemon is up: a CLI command beside a writer `serve`) refuses the keyword lane naming the fix,
/// as busy (the adapter marks it so the CLI exits `3`), instead of an internal error.
#[test]
fn the_keyword_lane_without_an_index_is_refused_with_the_fix() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), BODY).unwrap();
    drop(scanned(root));
    let store = Store::open_read_only_no_index(root, crate::store::VIEW_WORKING).expect("index-less store");
    assert!(!store.rescan_owed(), "the scanned index owes no rebuild");
    let ws = Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).unwrap()),
        None,
        crate::engine::workspace::OpenOptions {
            background: false,
            read_only: true,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            #[cfg(feature = "memory")]
            memory_owner: |_| "owner".to_string(),
        },
    );

    let refused =
        keyword_hits(&ws, "retry upload", 5).expect_err("a session with no index cannot run the keyword lane");
    let EngineError::Busy(message) = refused else {
        panic!("busy, so the CLI exits 3: {refused:?}");
    };
    assert!(message.contains("holds this workspace's index"), "{message}");
    #[cfg(all(feature = "comms", any(unix, windows)))]
    assert!(message.contains("`basemind comms start`"), "{message}");
}

/// The refusal of a semantic-only search names the argument the tool takes: an agent that follows
/// it with `mode` gets an unknown-argument error (review 3 S3).
#[test]
fn the_semantic_refusal_names_the_lane_argument() {
    let meanwhile = SEMANTIC_MEANWHILE.expect("the refusal says what to do meanwhile");
    assert!(meanwhile.contains("`lane: \"keyword\"`"), "{meanwhile}");
    assert!(meanwhile.contains("`lane: \"hybrid\"`"), "{meanwhile}");
    assert!(!meanwhile.contains("mode"), "{meanwhile}");
    let followed: Result<crate::engine::types::search::CodeSearchParams, _> =
        serde_json::from_value(serde_json::json!({"query": "retry", "lane": "keyword"}));
    assert!(followed.is_ok(), "the call the refusal suggests is accepted");
}

fn ranking(unavailable: Vec<LaneUnavailable>) -> CodeSearchRanking {
    CodeSearchRanking {
        query: "q".to_string(),
        truncated_reason: None,
        omitted: 0,
        hits: vec![seed("c".to_string())],
        lanes_ran: vec!["keyword".to_string(), "exact".to_string()],
        lanes_unavailable: unavailable,
    }
}

/// A lane the configuration turned off is not a degraded answer: with the default `embed = false`
/// a hybrid ranking is kept for its pages and repeats. A lane that failed, or a reranker that did
/// not run, still is (review 3 S6).
#[test]
fn a_ranking_without_the_lane_the_config_turned_off_is_kept() {
    let off = LaneUnavailable {
        lane: "vector".to_string(),
        reason: EMBEDDINGS_OFF.to_string(),
    };
    assert!(is_cacheable(&ranking(vec![off])));
    let failed = LaneUnavailable {
        lane: "vector".to_string(),
        reason: "the vector lane failed (no model)".to_string(),
    };
    assert!(!is_cacheable(&ranking(vec![failed])));
    let rerank = LaneUnavailable {
        lane: "rerank".to_string(),
        reason: "the reranker failed: offline".to_string(),
    };
    assert!(!is_cacheable(&ranking(vec![rerank])));
    let mut empty = ranking(Vec::new());
    empty.hits.clear();
    assert!(!is_cacheable(&empty));
}
