use super::*;

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

fn request(query: &str, kind: Option<&str>, keywords: Option<&str>, path: Option<&str>) -> DocsRequest {
    DocsRequest {
        query: query.to_string(),
        kind: kind.map(str::to_string),
        keywords: keywords.map(str::to_string),
        path: path.map(str::to_string),
        ..DocsRequest::default()
    }
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
    let unknown_kind = Filters::parse(&request("q", Some("slides"), None, None), None);
    assert!(matches!(unknown_kind, Err(DocsSearchError::InvalidArgument(text)) if text.contains("markdown")));
    let bad_glob = Filters::parse(&request("q", None, None, Some("docs/[*")), None);
    assert!(matches!(bad_glob, Err(DocsSearchError::InvalidArgument(text)) if text.contains("read as a glob")));
    let entity = DocsRequest {
        entity: Some("person".to_string()),
        ..request("q", None, None, None)
    };
    assert!(matches!(
        Filters::parse(&entity, Some("no NER".to_string())),
        Err(DocsSearchError::Unsupported(_))
    ));
    assert!(matches!(
        Filters::parse(&request(" ", None, None, None), None),
        Err(DocsSearchError::InvalidArgument(_))
    ));
    let ok = Filters::parse(
        &request("q", Some("PDF"), Some("Flood, Barrier ,"), Some("docs/**/*.pdf")),
        None,
    )
    .unwrap();
    assert_eq!(ok.kind.as_deref(), Some("pdf"));
    assert_eq!(ok.keywords, vec!["flood".to_string(), "barrier".to_string()]);
    assert!(ok.path.as_ref().unwrap().matches(b"docs/a/b.pdf"));
    assert!(!ok.path.as_ref().unwrap().matches(b"other/b.pdf"));
    // Brackets without a `*` are literal: the filter is the prefix of the Next.js-style route.
    let prefix = Filters::parse(&request("q", None, None, Some("docs/[id]/")), None).unwrap();
    assert!(prefix.path.as_ref().unwrap().matches(b"docs/[id]/notes.md"));
    assert!(!prefix.path.as_ref().unwrap().matches(b"docs/i/notes.md"));
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
    assert!(!is_cacheable(&ranking));
}

fn ranked_chunk(doc: &str, ord: u32, text: &str) -> RankedChunk {
    RankedChunk {
        doc: doc.to_string(),
        ord,
        paths: vec![format!("{doc}.md"), format!("copy/{doc}.md")],
        mime_type: "text/markdown".to_string(),
        kind: "unknown".to_string(),
        cite: DocCite {
            page_start: None,
            page_end: None,
            section: Some(vec!["Guide".to_string(), format!("Part {ord}")]),
            line_start: Some(ord * 10 + 1),
            line_end: Some(ord * 10 + 9),
            byte_start: ord * 100,
            byte_end: ord * 100 + 120,
            headings: Vec::new(),
        },
        text: text.to_string(),
        score: 1.0 / f32::from(u16::try_from(ord).unwrap() + 1),
        lanes: LaneRanks::default(),
        rerank_score: None,
    }
}

fn scored(index: usize, score: f32) -> xberg::RerankedDocument {
    xberg::RerankedDocument {
        index,
        score,
        document: String::new(),
    }
}

#[test]
fn the_reranker_reorders_its_candidates_and_drops_nothing() {
    let mut hits: Vec<RankedChunk> = (0..5).map(|i| ranked_chunk("d", i, &i.to_string())).collect();
    let answer = vec![scored(2, 0.9), scored(0, 0.5)];
    reorder(&mut hits, 3, &answer).unwrap();
    let order: Vec<u32> = hits.iter().map(|h| h.ord).collect();
    assert_eq!(order, vec![2, 0, 1, 3, 4]);
    assert_eq!(hits[0].rerank_score, Some(0.9));
    assert_eq!(hits[2].rerank_score, None);

    let mut hits: Vec<RankedChunk> = (0..3).map(|i| ranked_chunk("d", i, "x")).collect();
    let bad = vec![scored(7, 0.1)];
    assert!(reorder(&mut hits, 2, &bad).is_err());
    assert_eq!(hits.len(), 3, "a bad answer keeps every hit");
}

/// A reranker answer with an index out of range leaves the fused order exactly as it was, with no
/// hit carrying a rerank score: it put the unscored candidates before the ones already scored.
#[test]
fn a_bad_reranker_answer_keeps_the_fused_order() {
    let mut hits: Vec<RankedChunk> = (0..4).map(|i| ranked_chunk("d", i, "x")).collect();
    let bad = vec![scored(2, 0.9), scored(7, 0.1)];
    assert!(reorder(&mut hits, 3, &bad).is_err());
    let order: Vec<u32> = hits.iter().map(|h| h.ord).collect();
    assert_eq!(order, vec![0, 1, 2, 3]);
    assert!(hits.iter().all(|h| h.rerank_score.is_none()));
}
