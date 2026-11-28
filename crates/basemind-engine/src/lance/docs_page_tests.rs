use super::*;
use crate::lance::docs_types::LaneRanks;

fn ranked(doc: &str, ord: u32, text: &str) -> RankedChunk {
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

#[test]
fn adjacent_chunks_of_one_document_become_one_hit_at_the_best_rank() {
    let groups = merge(vec![
        ranked("a", 4, "four"),
        ranked("a", 3, "three"),
        ranked("b", 1, "one"),
        ranked("a", 7, "seven"),
    ]);
    let spans: Vec<(&str, (u32, u32))> = groups.iter().map(|g| (g.first.doc.as_str(), g.ord_range())).collect();
    assert_eq!(spans, vec![("a", (3, 4)), ("b", (1, 1)), ("a", (7, 7))]);
    let hit = hit_of(&groups[0], Some("Guide".to_string()), Some(9));
    assert_eq!(hit.text, "three\nfour");
    assert_eq!((hit.chunk.ord, hit.chunk.ord_end, hit.chunk.of), (3, 4, Some(9)));
    assert_eq!((hit.cite.line_start, hit.cite.line_end), (Some(31), Some(49)));
    assert_eq!((hit.cite.byte_start, hit.cite.byte_end), (300, 520));
    assert_eq!(hit.cite.section, Some(vec!["Guide".to_string(), "Part 3".to_string()]));
    assert_eq!(hit.path, "a.md");
    assert_eq!(hit.also_at, vec!["copy/a.md".to_string()]);
}

#[test]
fn stitching_drops_the_overlap_between_consecutive_chunks() {
    assert_eq!(
        stitch("alpha beta gamma epsilon", "beta gamma epsilon delta"),
        "alpha beta gamma epsilon delta"
    );
    assert_eq!(stitch("the", "end"), "the\nend", "a one-letter match is not an overlap");
    assert_eq!(stitch("one\n", "two"), "one\ntwo");
    assert_eq!(stitch("one", "two"), "one\ntwo");
}

#[test]
fn chunks_of_one_document_merge_wherever_they_rank() {
    // A0 and A1 are adjacent chunks with B0 between them in rank: they are one hit at A0's rank.
    let groups = merge(vec![ranked("a", 0, "x"), ranked("b", 0, "y"), ranked("a", 1, "z")]);
    let spans: Vec<(&str, (u32, u32))> = groups.iter().map(|g| (g.first.doc.as_str(), g.ord_range())).collect();
    assert_eq!(spans, vec![("a", (0, 1)), ("b", (0, 0))]);
    let groups = merge(vec![ranked("a", 2, "x"), ranked("a", 0, "y"), ranked("a", 1, "z")]);
    let spans: Vec<(u32, u32)> = groups.iter().map(Merged::ord_range).collect();
    assert_eq!(spans, vec![(0, 2)]);
    assert_eq!(groups[0].first.ord, 2, "the group ranks where its best chunk did");
}

#[test]
fn a_chunk_ranked_twice_is_one_member() {
    let groups = merge(vec![ranked("a", 1, "x"), ranked("a", 1, "x")]);
    assert_eq!(groups[0].members.len(), 1);
}

#[test]
fn a_document_past_its_cap_ranks_behind_the_others_and_nothing_is_dropped() {
    let ranking = || {
        merge(vec![
            ranked("a", 0, "a0"),
            ranked("a", 5, "a5"),
            ranked("a", 9, "a9"),
            ranked("b", 0, "b0"),
            ranked("c", 0, "c0"),
        ])
    };
    let order = |groups: Vec<Merged>| -> Vec<(String, u32)> {
        groups.iter().map(|g| (g.first.doc.clone(), g.first.ord)).collect()
    };
    let doc = |name: &str, ord: u32| (name.to_string(), ord);
    assert_eq!(
        order(diversify(ranking(), 2)),
        vec![doc("a", 0), doc("a", 5), doc("b", 0), doc("c", 0), doc("a", 9)]
    );
    assert_eq!(
        order(diversify(ranking(), 1)),
        vec![doc("a", 0), doc("b", 0), doc("c", 0), doc("a", 5), doc("a", 9)]
    );
    assert_eq!(order(diversify(ranking(), 0)), order(ranking()), "0 is no cap");
}

fn ranking_of(hits: Vec<RankedChunk>) -> DocsRanking {
    DocsRanking {
        query: "q".to_string(),
        hits,
        ..DocsRanking::default()
    }
}

fn paging(offset: u32, limit: u32) -> Paging {
    Paging {
        offset,
        limit,
        max_tokens: None,
        per_doc: 0,
        context: false,
    }
}

/// Four chunks of one document that overlap by a sentence, as xberg cuts them.
fn overlapping_chunks() -> Vec<RankedChunk> {
    let sentences = [
        "Branches are named after the ticket key.",
        "Commit subjects start with the key.",
        "A subject stays under one hundred characters.",
        "Bodies are written only on request.",
        "Merges are squashed into one commit.",
    ];
    (0..4)
        .map(|ord| {
            ranked(
                "adr",
                ord,
                &format!("{} {}", sentences[ord as usize], sentences[ord as usize + 1]),
            )
        })
        .collect()
}

#[test]
fn overlapping_chunks_of_one_document_are_one_hit_with_no_repeated_text() {
    let chunks = overlapping_chunks();
    let separate: usize = chunks
        .iter()
        .map(|chunk| {
            let group = Merged {
                first: chunk.clone(),
                members: vec![chunk.clone()],
            };
            serde_json::to_vec(&hit_of(&group, None, None)).unwrap().len()
        })
        .sum();
    // Ranked out of order, with another document between them.
    let ranking = vec![
        chunks[1].clone(),
        ranked("other", 0, "unrelated text of another document"),
        chunks[3].clone(),
        chunks[0].clone(),
        chunks[2].clone(),
    ];
    let page = page(None, ranking_of(ranking), paging(0, 10));
    assert_eq!(page.hits.len(), 2);
    let hit = &page.hits[0];
    assert_eq!((hit.chunk.ord, hit.chunk.ord_end), (0, 3));
    assert_eq!(
        hit.text,
        "Branches are named after the ticket key. Commit subjects start with the key. \
         A subject stays under one hundred characters. Bodies are written only on request. \
         Merges are squashed into one commit."
    );
    for sentence in [
        "Commit subjects start with the key.",
        "A subject stays under one hundred characters.",
    ] {
        assert_eq!(hit.text.matches(sentence).count(), 1, "{sentence}");
    }
    let merged = serde_json::to_vec(hit).unwrap().len();
    assert!(
        merged < separate / 2,
        "{merged} bytes merged against {separate} separate"
    );
}

#[test]
fn the_page_counts_merged_hits_and_resumes_after_the_last_one() {
    let ranking = vec![
        ranked("a", 0, "a0 text"),
        ranked("b", 0, "b0 text"),
        ranked("a", 1, "a1 text"),
        ranked("c", 0, "c0 text"),
    ];
    let first = page(None, ranking_of(ranking.clone()), paging(0, 2));
    let docs: Vec<&str> = first.hits.iter().map(|h| h.chunk.doc.as_str()).collect();
    assert_eq!(docs, ["a", "b"]);
    assert_eq!(first.next_offset, Some(2));
    let second = page(None, ranking_of(ranking), paging(2, 2));
    let docs: Vec<&str> = second.hits.iter().map(|h| h.chunk.doc.as_str()).collect();
    assert_eq!(docs, ["c"]);
    assert_eq!(second.next_offset, None);
}

#[test]
fn a_limit_cut_of_a_deep_ranking_is_paging_not_truncation() {
    let deep: Vec<RankedChunk> = (0..=RANKED_DEPTH as u32)
        .map(|n| ranked(&format!("doc{n}"), 0, "text"))
        .collect();
    let first = page(None, ranking_of(deep.clone()), paging(0, 5));
    assert_eq!(first.hits.len(), 5);
    assert_eq!(first.next_offset, Some(5));
    assert!(!first.truncated);
    assert_eq!(first.truncated_reason, None);

    let last = page(None, ranking_of(deep), paging(RANKED_DEPTH as u32 - 5, 5));
    assert_eq!(last.hits.len(), 5);
    assert_eq!(last.next_offset, None);
    assert!(last.truncated, "the depth is where paging stops");
    assert_eq!(last.truncated_reason.as_deref(), Some("depth"));
}

#[test]
fn per_doc_lets_a_second_document_onto_the_first_page() {
    let ranking = vec![
        ranked("a", 0, "a0"),
        ranked("a", 4, "a4"),
        ranked("a", 8, "a8"),
        ranked("b", 0, "b0"),
    ];
    let capped = Paging {
        per_doc: 1,
        ..paging(0, 2)
    };
    let docs = |page: &DocsPage| -> Vec<String> { page.hits.iter().map(|h| h.chunk.doc.clone()).collect() };
    assert_eq!(docs(&page(None, ranking_of(ranking.clone()), capped)), ["a", "b"]);
    assert_eq!(docs(&page(None, ranking_of(ranking), paging(0, 2))), ["a", "a"]);
}

#[test]
fn a_hit_names_the_headings_that_start_inside_it() {
    let text = "intro line\n## 5. Staffing\ncrew\n```sh\n# not a heading\n```\n### 7. Recovery ###\n#hashtag\nend";
    assert_eq!(
        headings_in(text),
        vec!["5. Staffing".to_string(), "7. Recovery".to_string()]
    );
    assert!(headings_in("no headings here").is_empty());
}
