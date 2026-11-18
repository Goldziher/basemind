//! The position and span helpers behind `code_refs` relation `definition`.

use super::*;

#[test]
fn line_col_to_offset_maps_positions() {
    let src = b"const count = 1;\nreturn count;\n";
    assert_eq!(line_col_to_offset(src, 1, 6), Some(6));
    let second = (src.iter().position(|&b| b == b'\n').unwrap() + 1 + "return ".len()) as u32;
    assert_eq!(line_col_to_offset(src, 2, 7), Some(second));
    assert_eq!(line_col_to_offset(src, 9, 0), None);
    assert_eq!(
        line_col_to_offset(src, 1, 17),
        None,
        "col past EOL must be None, not next line"
    );
    assert_eq!(line_col_to_offset(src, 1, 16), Some(16));
}

#[test]
fn identifier_at_reads_the_token() {
    let src = b"const count = 1;";
    assert_eq!(identifier_at(src, 6), "count");
    assert_eq!(identifier_at(b"let a_1$ = 2;", 4), "a_1$");
    assert_eq!(identifier_at(src, 5), "");
    assert_eq!(identifier_at(src, 999), "");
}

#[test]
fn offset_to_line_col_roundtrips() {
    let src = b"a\nbb\nccc";
    assert_eq!(offset_to_line_col(src, 0), (1, 0));
    assert_eq!(offset_to_line_col(src, 2), (2, 0));
    assert_eq!(offset_to_line_col(src, 5), (3, 0));
    assert_eq!(offset_to_line_col(src, 999), (3, 3));
}

#[test]
fn containing_edge_respects_spans() {
    let zero_width = ResolvedEdge {
        use_start: 10,
        use_end: 10,
        def_start: 0,
        def_end: 0,
    };
    assert!(containing_edge(std::slice::from_ref(&zero_width), 10).is_some());
    assert!(containing_edge(std::slice::from_ref(&zero_width), 11).is_none());

    let spanned = ResolvedEdge {
        use_start: 20,
        use_end: 25,
        def_start: 0,
        def_end: 5,
    };
    assert!(containing_edge(std::slice::from_ref(&spanned), 20).is_some());
    assert!(containing_edge(std::slice::from_ref(&spanned), 24).is_some());
    assert!(containing_edge(std::slice::from_ref(&spanned), 25).is_none());
}

#[cfg(all(feature = "comms", feature = "code-intel-js", any(unix, windows)))]
#[path = "helpers_intel_hosted_tests.rs"]
mod hosted;
