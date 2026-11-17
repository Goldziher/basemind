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
fn only_rank_neighbours_merge_so_a_page_can_end_after_any_hit() {
    // A0 and A1 are adjacent chunks but B0 sits between them in rank: three hits, not two, so a
    // budget that keeps the first hit resumes at B0.
    let groups = merge(vec![ranked("a", 0, "x"), ranked("b", 0, "y"), ranked("a", 1, "z")]);
    assert_eq!(groups.len(), 3);
    let groups = merge(vec![ranked("a", 2, "x"), ranked("a", 0, "y"), ranked("a", 1, "z")]);
    let spans: Vec<(u32, u32)> = groups.iter().map(Merged::ord_range).collect();
    assert_eq!(spans, vec![(2, 2), (0, 1)]);
}
