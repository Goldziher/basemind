use super::*;

/// Every span is a line range of `text` whose lines hold exactly the span's text, end to end.
fn assert_cited(text: &str, spans: &[(usize, usize)]) {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    for (&(start, end), (first, last)) in spans.iter().zip(line_ranges(text, spans)) {
        let cited: String = lines[first as usize - 1..last as usize].concat();
        assert!(
            cited.contains(text[start..end].trim()),
            "lines {first}-{last} do not hold the chunk {:?}",
            &text[start..end]
        );
    }
}

#[test]
fn shapes_follow_the_mime_type() {
    assert_eq!(shape("text/plain; charset=utf-8"), Some(Shape::Prose));
    assert_eq!(shape("text/csv"), Some(Shape::Records { header: true }));
    assert_eq!(shape("application/x-ndjson"), Some(Shape::Records { header: false }));
    for tree in [
        "application/json",
        "application/yaml",
        "text/x-yaml",
        "application/toml",
    ] {
        assert_eq!(shape(tree), Some(Shape::Tree), "{tree}");
    }
    for other in ["text/markdown", "text/html", "application/pdf", "application/xml"] {
        assert_eq!(shape(other), None, "{other}");
    }
}

#[test]
fn a_log_is_cut_at_line_ends_and_every_chunk_cites_its_lines() {
    let log: String = (0..400)
        .map(|i| format!("2026-11-17 10:{:02}:{:02} host svc[{i}]: event ok\n", i % 60, i % 59))
        .collect();
    let spans = spans(&log, Shape::Prose, 1000, 200);
    assert!(spans.len() > 10);
    for &(start, end) in &spans {
        assert!(log[start..end].chars().count() <= 1000);
        assert!(start == 0 || log.as_bytes()[start - 1] == b'\n', "starts a line");
        assert!(end == log.len() || log.as_bytes()[end - 1] == b'\n', "ends a line");
    }
    assert!(spans.windows(2).all(|w| w[1].0 < w[0].1), "prose chunks overlap");
    assert_eq!(spans.last().unwrap().1, log.len(), "the tail is covered");
    assert_cited(&log, &spans);
    let lines = line_ranges(&log, &spans);
    assert_eq!(lines[0].0, 1);
    assert_eq!(lines.last().unwrap().1, 400);
}

#[test]
fn prose_breaks_at_a_paragraph_before_a_line() {
    let para = "a line of prose that wraps\n".repeat(6);
    let text = format!("{para}\n{para}\n{para}");
    let spans = spans(&text, Shape::Prose, 300, 0);
    let first_end = spans[0].1;
    assert_eq!(&text[first_end - 2..first_end], "\n\n", "{:?}", &text[..first_end]);
    assert_cited(&text, &spans);
}

#[test]
fn records_do_not_overlap_and_repeat_the_header_only_for_embedding() {
    let csv: String = std::iter::once("id,name,amount\n".to_string())
        .chain((0..200).map(|i| format!("{i},item number {i},{}\n", i * 7)))
        .collect();
    let (chunks, dense) = chunks(&csv, Shape::Records { header: true }, 500, 200, true);
    assert!(chunks.len() > 5);
    assert!(
        chunks.windows(2).all(|w| w[0].byte_end == w[1].byte_start),
        "records are not repeated"
    );
    for (chunk, dense) in chunks.iter().zip(&dense) {
        assert_eq!(chunk.text, csv[chunk.byte_start as usize..chunk.byte_end as usize]);
        assert!(chunk.line_start.is_some() && chunk.line_end.is_some());
        assert!(dense.starts_with("id,name,amount\n"), "{dense:?}");
    }
    assert_eq!(dense[0], chunks[0].text, "the first chunk holds the header already");
    let (_, none) = super::chunks(&csv, Shape::Records { header: true }, 500, 0, false);
    assert!(none.is_empty());
}

#[test]
fn a_tree_breaks_before_a_top_level_entry_and_never_before_a_closer() {
    let yaml: String = (0..40)
        .map(|i| {
            format!(
                "service{i}:\n  image: registry/app:{i}\n  replicas: {}\n  env:\n    - KEY={i}\n",
                i % 3
            )
        })
        .collect();
    let spans = spans(&yaml, Shape::Tree, 400, 200);
    for &(start, _) in &spans[1..] {
        assert!(yaml[start..].starts_with("service"), "{:?}", &yaml[start..start + 20]);
    }
    assert!(spans.windows(2).all(|w| w[0].1 == w[1].0), "no overlap");
    assert_cited(&yaml, &spans);

    let json = format!(
        "{{\n{}\n}}\n",
        (0..40)
            .map(|i| format!("  \"key{i}\": {{\n    \"value\": {i},\n    \"list\": [1, 2]\n  }}"))
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let spans = super::spans(&json, Shape::Tree, 400, 0);
    for &(start, _) in &spans[1..] {
        assert!(json[start..].starts_with("  \"key"), "{:?}", &json[start..start + 20]);
    }
    assert_cited(&json, &spans);
}

#[test]
fn a_line_longer_than_the_window_is_cut_inside_it() {
    let minified = format!(
        "{{{}}}",
        (0..300)
            .map(|i| format!("\"k{i}\": {i}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let spans = spans(&minified, Shape::Tree, 200, 0);
    assert!(spans.len() > 5);
    let covered: usize = spans.iter().map(|&(s, e)| e - s).sum();
    assert_eq!(covered, minified.len());
    assert!(line_ranges(&minified, &spans).iter().all(|&lines| lines == (1, 1)));

    let wide = "é".repeat(500);
    let spans = super::spans(&wide, Shape::Prose, 100, 10);
    assert!(
        spans
            .iter()
            .all(|&(s, e)| wide.is_char_boundary(s) && wide.is_char_boundary(e))
    );
}

#[test]
fn chunking_is_linear_in_the_source() {
    // One 4 MB log with no line breaks inside the windows' back halves would be quadratic if a
    // window rescanned from its start; it must finish well inside a debug test's patience.
    let log = "x".repeat(4 * 1024 * 1024);
    let started = std::time::Instant::now();
    let spans = spans(&log, Shape::Prose, 1000, 200);
    assert_eq!(spans.len(), log.len().div_ceil(1000));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn entities_are_placed_where_the_source_says_them() {
    let source = "intro\nAda Lovelace wrote notes.\nlater Ada Lovelace again\n";
    let entity = |text: &str, start: u32| super::super::DocEntity {
        category: "person".into(),
        text: text.into(),
        start,
        end: start + 3,
        confidence: None,
    };
    let mut entities = vec![
        entity("Ada Lovelace", 0),
        entity("Ada Lovelace", 40),
        entity("Nobody", 50),
    ];
    place_entities(source, &mut entities);
    assert_eq!((entities[0].start, entities[0].end), (6, 18));
    assert_eq!(entities[1].start as usize, source.rfind("Ada").unwrap());
    assert_eq!(
        entities[2].start,
        u32::MAX,
        "an entity not in the source is in no chunk"
    );
}

/// A file that is not UTF-8 (UTF-16 with its BOM, Latin-1) is not chunked from its bytes: xberg's
/// decoded text is.
#[test]
fn a_file_that_is_not_utf8_is_left_to_xberg() {
    let dir = tempfile::tempdir().unwrap();
    let utf16 = dir.path().join("utf16.log");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend("first line\nsecond line\n".encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&utf16, bytes).unwrap();
    let latin1 = dir.path().join("latin1.txt");
    std::fs::write(&latin1, b"caf\xe9 au lait\n").unwrap();
    let utf8 = dir.path().join("utf8.txt");
    std::fs::write(&utf8, "café au lait\n").unwrap();

    assert_eq!(read(&utf16), None);
    assert_eq!(read(&latin1), None);
    assert_eq!(read(&utf8).as_deref(), Some("café au lait\n"));
}
