//! A Java field is a constant exactly when it is both `static` and `final`, whichever shape the
//! grammar gives its modifiers.

use super::SymbolKind;
use super::l1::extract_l1;

#[test]
fn java_static_final_field_is_a_const_in_either_keyword_order() {
    for (decl, expected) in [
        ("public static final int A = 1;", SymbolKind::Const),
        ("final static int A = 1;", SymbolKind::Const),
        ("@Deprecated private static final int A = 1;", SymbolKind::Const),
        ("static int A = 1;", SymbolKind::Field),
        ("final int A = 1;", SymbolKind::Field),
        ("int A = 1;", SymbolKind::Field),
    ] {
        let src = format!("class C {{ {decl} }}");
        let map = extract_l1("java", src.as_bytes()).expect("extract l1");
        let sym = map
            .symbols
            .iter()
            .find(|s| s.name == "A")
            .unwrap_or_else(|| panic!("no symbol `A` for `{decl}` in {:?}", map.symbols));
        assert_eq!(sym.kind, expected, "{decl}");
    }
}
