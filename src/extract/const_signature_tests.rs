//! A constant's outline entry reads `const NAME = value`, covers only its own member, and is cut
//! per member, in every language that outlines constants.

use super::SymbolKind;
use super::l1::extract_l1;

/// `(signature, source text of the symbol's span)` of the symbol called `name`.
fn const_entry(lang: &'static str, src: &str, name: &str) -> (String, String) {
    let map = extract_l1(lang, src.as_bytes()).expect("extract l1");
    let sym = map
        .symbols
        .iter()
        .find(|s| s.name == name && s.kind == SymbolKind::Const)
        .unwrap_or_else(|| panic!("no const `{name}` in {:?}", map.symbols));
    let span = &src[sym.start_byte as usize..sym.end_byte as usize];
    (sym.signature.clone().unwrap_or_default(), span.to_string())
}

#[test]
fn typescript_const_keeps_its_name() {
    let src = "const FooPage = lazy(() => import('@/pages/recipes'));\n";
    for lang in ["typescript", "tsx", "javascript"] {
        let (sig, _) = const_entry(lang, src, "FooPage");
        assert_eq!(sig, "const FooPage = lazy(() => import('@/pages/recipes'))", "{lang}");
    }
}

#[test]
fn typescript_declaration_with_several_members_splits_per_member() {
    let src = "const first = 1, second = 'two';\n";
    assert_eq!(const_entry("typescript", src, "first").0, "const first = 1");
    assert_eq!(const_entry("typescript", src, "second").0, "const second = 'two'");
}

#[test]
fn typescript_const_keeps_its_type_annotation() {
    let (sig, _) = const_entry("typescript", "export const LIMIT: number = 5;\n", "LIMIT");
    assert_eq!(sig, "const LIMIT: number = 5");
}

#[test]
fn long_const_value_is_truncated_per_member() {
    let long = "x".repeat(300);
    let (sig, _) = const_entry("typescript", &format!("const BIG = make('{long}');\n"), "BIG");
    assert!(sig.starts_with("const BIG = make('xxx"), "{sig}");
    assert!(sig.ends_with('…'), "{sig}");
    assert_eq!(sig.chars().count(), 80 + 1);
}

#[test]
fn go_const_group_gives_each_member_its_own_signature_and_span() {
    let src = "package p\n\nconst (\n\tsocksVersion = 5\n\tauthNone     = 0x02\n\tname  string = \"x\"\n)\n";
    assert_eq!(
        const_entry("go", src, "socksVersion"),
        ("const socksVersion = 5".to_string(), "socksVersion = 5".to_string())
    );
    assert_eq!(
        const_entry("go", src, "authNone"),
        ("const authNone = 0x02".to_string(), "authNone     = 0x02".to_string())
    );
    assert_eq!(
        const_entry("go", src, "name"),
        (
            "const name string = \"x\"".to_string(),
            "name  string = \"x\"".to_string()
        )
    );
}

#[test]
fn go_single_const_and_var_keep_their_names() {
    let src = "package p\n\nconst Max = 3\n\nvar Default = New()\n";
    assert_eq!(const_entry("go", src, "Max").0, "const Max = 3");
    assert_eq!(const_entry("go", src, "Default").0, "var Default = New()");
}

#[test]
fn python_assignment_reads_as_a_const_declaration() {
    let src = "MAX_RETRIES = 3\n\nclass Row:\n    identification_source = sa.Column(sa.String)\n";
    assert_eq!(const_entry("python", src, "MAX_RETRIES").0, "const MAX_RETRIES = 3");
    assert_eq!(
        const_entry("python", src, "identification_source").0,
        "const identification_source = sa.Column(sa.String)"
    );
}

#[test]
fn const_group_outline_shrinks_to_one_short_signature_per_member() {
    let members: String = (0..15).map(|i| format!("\tm{i} = {i}\n")).collect();
    let src = format!("package p\n\nconst (\n{members})\n");
    let map = extract_l1("go", src.as_bytes()).expect("extract l1");
    let total: usize = map
        .symbols
        .iter()
        .filter_map(|s| s.signature.as_deref())
        .map(str::len)
        .sum();
    assert_eq!(map.symbols.len(), 15);
    assert!(
        total < 15 * "const m00 = 00".len() + 15,
        "outline signatures total {total} bytes"
    );
}
