//! Java's override `src/queries/java.scm` replaces the adapted TSLP `tags.scm` for every section
//! (ADR-0048), so its symbols, calls and implementations must match exactly what the fallback did.

use tree_sitter::{Query, Tree};

use crate::lang::{QueryKind, extract_section, language, try_get_query, tslp_tags_adapted, with_parser};

const FIXTURE: &str = include_str!("../../tests/fixtures/http/spring/OwnerController.java");

/// Every `(capture name, start byte, end byte)` the query yields on the fixture, sorted.
fn captures(query: &Query, tree: &Tree) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    super::query_walk::for_each_match(query, tree.root_node(), FIXTURE.as_bytes(), |m| {
        for cap in m.captures() {
            let name = super::capture_name(query, cap.index).to_string();
            out.push((name, cap.node.start_byte(), cap.node.end_byte()));
        }
    })
    .expect("query walk");
    out.sort();
    out
}

#[test]
fn java_override_matches_what_the_tslp_fallback_extracted() {
    let adapted = tslp_tags_adapted("java").expect("TSLP ships a java tags.scm");
    let grammar = language("java").expect("java grammar");
    let tree = with_parser("java", |p| p.parse(FIXTURE, None))
        .expect("java parser")
        .expect("parse");
    for kind in [QueryKind::Symbols, QueryKind::Calls, QueryKind::Implementations] {
        let fallback_src = extract_section(&adapted, kind.name()).expect("fallback section");
        let fallback = Query::new(&grammar, &fallback_src).expect("fallback compiles");
        let ours = try_get_query("java", kind)
            .expect("override compiles")
            .expect("override section");
        let (want, got) = (captures(&fallback, &tree), captures(&ours, &tree));
        assert!(!want.is_empty(), "the fixture exercises the {} section", kind.name());
        assert_eq!(got, want, "java {} section differs from the TSLP fallback", kind.name());
    }
}

#[test]
fn java_extraction_keeps_the_fallback_counts() {
    let l1 = super::l1::extract_l1("java", FIXTURE.as_bytes()).expect("l1");
    let l2 = super::l2::extract_l2("java", FIXTURE.as_bytes()).expect("l2");
    let code: Vec<_> = l1
        .symbols
        .iter()
        .filter(|s| s.kind != super::SymbolKind::Route)
        .map(|s| (s.name.as_str(), s.kind))
        .collect();
    // Measured on the fixture with the TSLP fallback, before `java.scm` existed.
    assert_eq!(code.len(), 13, "{code:?}");
    assert_eq!(l2.calls.len(), 13, "{:?}", l2.calls);
    assert_eq!(l1.implementations.len(), 1, "{:?}", l1.implementations);
}
