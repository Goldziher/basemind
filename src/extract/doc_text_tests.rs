use super::*;

fn decode_entities(text: &str) -> Cow<'_, str> {
    decode_entities_where(text, false)
}

#[test]
fn prose_entities_are_decoded_once() {
    assert_eq!(decode_entities("R&amp;D and a &lt;b&gt; tag"), "R&D and a <b> tag");
    assert_eq!(decode_entities("&amp;lt; stays one level"), "&lt; stays one level");
    assert_eq!(decode_entities("&#65;&#x42;&nbsp;x\u{a0}y\u{ad}z"), "AB x yz");
    assert_eq!(decode_entities("a & b; &unknown; &"), "a & b; &unknown; &");
}

#[test]
fn markdown_escapes_in_prose_are_read_as_their_character() {
    assert_eq!(
        decode_entities("the R\\&D group and a \\*star\\*"),
        "the R&D group and a *star*"
    );
    assert_eq!(decode_entities("a path C:\\dir stays"), "a path C:\\dir stays");
    assert_eq!(
        decode_entities("`a\\&b` and\n```\nre\\.x\n```\n"),
        "`a\\&b` and\n```\nre\\.x\n```\n"
    );
    assert_eq!(
        decode_entities_where("```\nre\\.x &gt;\n```\n", true),
        "```\nre\\.x >\n```\n"
    );
}

#[test]
fn text_without_entities_is_borrowed() {
    assert!(matches!(decode_entities("plain text"), Cow::Borrowed(_)));
    assert!(matches!(decode_entities("a & b"), Cow::Borrowed(_)));
}

#[test]
fn code_keeps_its_entities() {
    let text = "R&amp;D\n```html\n<p>&amp;</p>\n```\n    &lt;indented&gt;\nuse `&amp;` here, not &amp;\n";
    assert_eq!(
        decode_entities(text),
        "R&D\n```html\n<p>&amp;</p>\n```\n    &lt;indented&gt;\nuse `&amp;` here, not &\n"
    );
}

#[test]
fn fenced_blocks_are_found_with_their_opening_line() {
    let content = "intro\n```mermaid\nA --> B\nB --> C\n```\nafter\n~~~\nopen\n";
    let blocks = fenced_blocks(content);
    let open = content.find("```mermaid").unwrap();
    let close = content.find("after").unwrap();
    assert_eq!(blocks[0], (open, open + "```mermaid\n".len(), close));
    let tilde = content.find("~~~").unwrap();
    assert_eq!(blocks[1], (tilde, tilde + 4, content.len()));
}

#[test]
fn a_start_inside_a_block_the_previous_chunk_closed_moves_past_it() {
    let content = "intro\n```mermaid\nA --> B\nB --> C\n```\nafter the diagram\n";
    let blocks = fenced_blocks(content);
    let start = content.find("B --> C").unwrap();
    let close = content.find("after").unwrap();
    assert_eq!(snap_start(content, &blocks, start, close + 3), (close, None));
}

#[test]
fn a_start_inside_a_block_that_runs_on_reopens_it() {
    let content = "intro\n```mermaid\nA --> B\nB --> C\nC --> D\n```\n";
    let blocks = fenced_blocks(content);
    let start = content.find("B --> C").unwrap();
    let previous_end = content.find("C --> D").unwrap();
    assert_eq!(
        snap_start(content, &blocks, start, previous_end),
        (start, Some("```mermaid"))
    );
}

#[test]
fn an_overlap_start_moves_to_the_next_line_or_word() {
    let content = "first line of prose\nsecond line here\n";
    let blocks = fenced_blocks(content);
    let mid_word = content.find("ine of").unwrap();
    assert_eq!(
        snap_start(content, &blocks, mid_word, content.len()),
        (content.find("second").unwrap(), None)
    );
    let one_line = "alpha beta gamma delta";
    let start = one_line.find("eta").unwrap();
    assert_eq!(
        snap_start(one_line, &[], start, one_line.len()),
        (one_line.find("gamma").unwrap(), None)
    );
    // Outside the overlap nothing moves: that text is in no other chunk.
    assert_eq!(snap_start(one_line, &[], start, start), (start, None));
}

#[test]
fn chunks_cite_the_source_lines_they_came_from() {
    let source = "# Title\n\nFirst paragraph line one.\nline two.\n\n## Next\n\nSecond paragraph.\n";
    let lines = source_lines(
        source,
        [
            "Title\n\nFirst paragraph line one.\nline two.",
            "Next\n\nSecond paragraph.",
        ],
    );
    assert_eq!(lines, vec![Some((1, 4)), Some((6, 8))]);
    assert_eq!(source_lines(source, ["not in the source"]), vec![None]);
}

#[test]
fn a_reflowed_source_is_searched_in_linear_time() {
    // The extractor joined every line of the log into one, so no chunk's line is in the source.
    let source: String = (0..20_000)
        .map(|i| format!("2021-01-27 host svc[{i}]: event ok\n"))
        .collect();
    let content = source.replace('\n', " ");
    let chunks: Vec<&str> = content
        .as_bytes()
        .chunks(1000)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let (lines, searched) = source_lines_counted(&source, chunks.iter().copied());
    assert!(lines.iter().all(Option::is_none));
    let bound = (SOURCE_LINES_SPILL_FACTOR + 1) * source.len() + chunks.len() * (8 * 1000 + 4096);
    assert!(
        searched <= bound,
        "searched {searched} bytes of a {} byte source for {} chunks",
        source.len(),
        chunks.len()
    );
}

#[test]
fn a_chunk_past_a_large_stripped_region_still_finds_its_line() {
    let filler = "x".repeat(200_000);
    let source = format!("first line here\n<!--\n{filler}\n-->\nlast line here\n");
    let lines = source_lines(&source, ["first line here", "last line here"]);
    assert_eq!(lines, vec![Some((1, 1)), Some((5, 5))]);
}

#[test]
fn an_embedder_that_fails_leaves_the_chunks_without_vectors() {
    let inputs = vec!["a".to_string(), "b".to_string()];
    let mut chunks = vec![super::super::DocChunk::default(), super::super::DocChunk::default()];
    let failed = embed_chunks(&inputs, &mut chunks, |_| {
        Err("loading dense embedder: offline".to_string())
    });
    assert_eq!(failed, Err("loading dense embedder: offline".to_string()));
    assert!(chunks.iter().all(|c| c.embedding.is_empty()));
    let short = embed_chunks(&inputs, &mut chunks, |_| Ok((2, vec![vec![1.0, 0.0]])));
    assert!(short.is_err());
    assert!(
        chunks.iter().all(|c| c.embedding.is_empty()),
        "a short answer sets no vector"
    );
    assert_eq!(
        embed_chunks(&inputs, &mut chunks, |_| Ok((2, vec![vec![1.0, 0.0]; 2]))),
        Ok(2)
    );
    assert!(chunks.iter().all(|c| c.embedding.len() == 2));
}
