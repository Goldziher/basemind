use super::*;
use crate::mcp::types_documents::RerankReport;

fn hit(doc: &str, ord: u32) -> DocChunkHit {
    DocChunkHit {
        doc_key: doc.to_string(),
        chunk_ord: ord,
        mime_type: "text/markdown".to_string(),
        chunk_kind: "unknown".to_string(),
        page_start: None,
        page_end: None,
        section: None,
        line_start: None,
        line_end: None,
        byte_start: 0,
        byte_end: 1,
        text: String::new(),
    }
}

fn params(arguments: serde_json::Value) -> DocsSearchParams {
    serde_json::from_value(arguments).expect("arguments")
}

#[test]
fn allowed_keys_in_the_predicate_need_no_projection() {
    let predicate = DocPredicate {
        keys: Some(vec!["a".to_string()]),
        ..DocPredicate::default()
    };
    let mut asked = Vec::new();
    let (hits, capped) = project(&predicate, &AHashSet::new(), |k| {
        asked.push(k);
        Ok(vec![hit("a", 0)])
    })
    .unwrap();
    assert_eq!((hits.len(), capped, asked), (1, false, vec![LANE_CANDIDATES]));
}

#[test]
fn a_projection_widens_until_enough_live_hits_and_says_when_it_gave_up() {
    let allowed: AHashSet<String> = ["live".to_string()].into_iter().collect();
    let predicate = DocPredicate::default();
    // Ninety-nine dead rows in a hundred: every fetch is full, the live share never reaches the lane depth.
    let mut asked = Vec::new();
    let (hits, capped) = project(&predicate, &allowed, |k| {
        asked.push(k);
        Ok((0..k)
            .map(|i| hit(if i % 100 == 0 { "live" } else { "dead" }, i as u32))
            .collect())
    })
    .unwrap();
    assert!(capped, "the cap was reached short of {LANE_CANDIDATES} live hits");
    assert_eq!(asked.first(), Some(&600));
    assert_eq!(asked.last(), Some(&LIVE_WIDEN_CAP));
    assert!(hits.iter().all(|h| h.doc_key == "live"));

    // A store that runs out of rows is not a cut.
    let (hits, capped) = project(&predicate, &allowed, |_| Ok(vec![hit("live", 0), hit("dead", 1)])).unwrap();
    assert_eq!((hits.len(), capped), (1, false));
}

#[test]
fn filters_are_checked_before_anything_runs() {
    let unknown_kind = Filters::of(&params(serde_json::json!({"query": "q", "kind": "slides"})));
    assert!(unknown_kind.err().unwrap().message.contains("markdown"));
    let bad_glob = Filters::of(&params(serde_json::json!({"query": "q", "path": "docs/[x"})));
    assert!(bad_glob.err().unwrap().message.contains("glob"));
    let ok = Filters::of(&params(serde_json::json!({
        "query": "q", "kind": "PDF", "keywords": "Flood, Barrier ,", "path": "docs/**/*.pdf"
    })))
    .unwrap();
    assert_eq!(ok.kind.as_deref(), Some("pdf"));
    assert_eq!(ok.keywords, vec!["flood".to_string(), "barrier".to_string()]);
    assert!(ok.path.as_ref().unwrap().is_match("docs/a/b.pdf"));
    assert!(!ok.path.as_ref().unwrap().is_match("other/b.pdf"));
}

#[test]
fn lanes_fuse_on_chunk_identity_and_carry_their_ranks() {
    let mut live = LiveDocs::default();
    live.by_key.insert("a".to_string(), vec!["a.md".to_string()]);
    live.by_key.insert("b".to_string(), vec!["b.md".to_string()]);
    let lanes = vec![
        (LANE_VECTOR, vec![hit("a", 1), hit("b", 0)]),
        (LANE_LEXICAL, vec![hit("b", 0), hit("gone", 0)]),
    ];
    let fused = fuse(&lanes, &live);
    let order: Vec<(&str, u32)> = fused.iter().map(|h| (h.doc.as_str(), h.ord)).collect();
    assert_eq!(
        order,
        vec![("b", 0), ("a", 1)],
        "a chunk of a document no longer live is dropped"
    );
    assert_eq!(
        fused[0].lanes,
        LaneRanks {
            vector: Some(2),
            lexical: Some(1)
        }
    );
}

#[test]
fn a_degraded_ranking_is_not_kept() {
    let mut ranking = DocsRanking {
        hits: vec![RankedChunk {
            doc: "a".to_string(),
            ord: 0,
            paths: vec!["a.md".to_string()],
            mime_type: String::new(),
            kind: String::new(),
            cite: DocCite::default(),
            text: String::new(),
            score: 1.0,
            lanes: LaneRanks::default(),
            rerank_score: None,
        }],
        ..DocsRanking::default()
    };
    assert!(is_cacheable(&ranking));
    ranking.rerank = RerankReport {
        ran: false,
        candidates: None,
        reason: Some("model".to_string()),
    };
    assert!(rerank_failed(&ranking.rerank));
    assert!(!is_cacheable(&ranking));
}
