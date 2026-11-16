//! Rust extraction cases a `syn`-based gold extractor found missing from the code map.

use basemind::extract::l1::extract_l1;
use basemind::extract::l2::extract_l2;
use basemind::extract::{FileMapL1, SymbolKind};

fn l1(src: &str) -> FileMapL1 {
    extract_l1("rust", src.as_bytes()).expect("extract l1")
}

fn callees(src: &str) -> Vec<String> {
    extract_l2("rust", src.as_bytes())
        .expect("extract l2")
        .calls
        .into_iter()
        .map(|c| c.callee)
        .collect()
}

fn impl_pairs(map: &FileMapL1) -> Vec<(&str, &str)> {
    map.implementations
        .iter()
        .map(|i| (i.trait_name.as_str(), i.impl_type.as_str()))
        .collect()
}

#[test]
fn trait_impls_with_path_or_generic_trait_are_keyed_by_last_segment() {
    let src = "\
impl<T> Extend<T> for Vec<T> {}
impl crate::scanner::ScanObserver for RescanPaths {}
impl<T> a::b::Sink<T> for c::Pipe<T> {}
impl Display for Plain {}
";
    let map = l1(src);
    let pairs = impl_pairs(&map);
    for want in [
        ("Extend", "Vec"),
        ("ScanObserver", "RescanPaths"),
        ("Sink", "Pipe"),
        ("Display", "Plain"),
    ] {
        assert!(pairs.contains(&want), "{want:?} missing from {pairs:?}");
    }
    assert_eq!(pairs.len(), 4, "one implementation per impl block: {pairs:?}");
    let extend = &map.implementations[0];
    assert_eq!((extend.start_byte, extend.start_row, extend.start_col), (0, 0, 0));
}

#[test]
fn calls_inside_macro_arguments_are_lexical_call_sites() {
    let src = "\
fn t() {
    assert!(is_dead_pane_status(x));
    assert_eq!(a::b::qualified(1), y.method(2), \"{}\", nested(inner(3)));
    debug_assert!(!negated(z));
}
";
    let calls = callees(src);
    for want in [
        "is_dead_pane_status",
        "qualified",
        "method",
        "nested",
        "inner",
        "negated",
    ] {
        assert!(calls.iter().any(|c| c == want), "{want} missing from {calls:?}");
    }
    for macro_name in ["assert", "assert_eq", "debug_assert"] {
        assert!(
            calls.iter().any(|c| c == macro_name),
            "{macro_name} missing from {calls:?}"
        );
    }
}

#[test]
fn lexical_macro_calls_exclude_definitions_nested_macros_and_macro_rules() {
    let src = "\
cfg_time! {
    pub(crate) fn caller_location() -> u32 { helper(1) }
    struct Wrapper(u32);
}
fn t() {
    println!(\"{}\", format!(\"{}\", value));
    let v = vec![build(1)];
}
macro_rules! m { ($x:expr) => { in_rules($x) }; }
";
    let calls = callees(src);
    assert!(calls.iter().any(|c| c == "helper"), "{calls:?}");
    assert!(calls.iter().any(|c| c == "build"), "{calls:?}");
    for bad in ["caller_location", "Wrapper", "format", "value", "in_rules"] {
        assert!(!calls.iter().any(|c| c == bad), "{bad} must not be a call: {calls:?}");
    }
}

#[test]
fn lexical_call_positions_point_at_the_callee() {
    let src = "fn t() {\n    assert!(is_ok(x));\n}\n";
    let l2 = extract_l2("rust", src.as_bytes()).expect("extract l2");
    let call = l2.calls.iter().find(|c| c.callee == "is_ok").expect("is_ok call");
    assert_eq!((call.start_row, call.start_col), (1, 12));
    assert_eq!(&src[call.start_byte as usize..call.end_byte as usize], "is_ok(x)");
}

#[test]
fn turbofish_calls_are_recorded() {
    let src = "fn t() { let n = s.parse::<u32>(); let v = collect::<Vec<u8>>(); let w = a::b::make::<T>(); }\n";
    let calls = callees(src);
    for want in ["parse", "collect", "make"] {
        assert!(calls.iter().any(|c| c == want), "{want} missing from {calls:?}");
    }
}

#[test]
fn items_inside_item_position_macro_bodies_are_symbols() {
    let src = "\
cfg_time! {
    #[track_caller]
    pub(crate) fn caller_location() -> u32 { 0 }

    pub struct Sleep;
    use std::time::Duration;
}
mod inner {
    cfg_rt! {
        cfg_net! {
            pub fn deep() {}
        }
    }
}
";
    let map = l1(src);
    let sym = map
        .symbols
        .iter()
        .find(|s| s.name == "caller_location")
        .expect("caller_location symbol");
    assert_eq!(sym.kind, SymbolKind::Function);
    assert_eq!((sym.start_row, sym.start_col), (2, 4));
    let body = &src[sym.start_byte as usize..sym.end_byte as usize];
    assert!(body.starts_with("pub(crate) fn caller_location"), "{body}");
    assert!(body.ends_with("{ 0 }"), "{body}");
    assert_eq!(sym.signature.as_deref(), Some("pub(crate) fn caller_location() -> u32"));

    let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
    for want in ["Sleep", "inner", "deep"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    let deep = map.symbols.iter().find(|s| s.name == "deep").expect("deep");
    assert_eq!((deep.start_row, deep.start_col), (10, 12));
    assert!(src[deep.start_byte as usize..].starts_with("pub fn deep"));
    assert!(
        map.imports
            .iter()
            .any(|i| i.raw == "use std::time::Duration;" && src[i.start_byte as usize..i.end_byte as usize] == i.raw),
        "{:?}",
        map.imports
    );
    let starts: Vec<u32> = map.symbols.iter().map(|s| s.start_byte).collect();
    let mut sorted = starts.clone();
    sorted.sort_unstable();
    assert_eq!(starts, sorted, "symbols stay in document order");
}

#[test]
fn only_item_position_macro_bodies_that_parse_cleanly_add_symbols() {
    let src = "\
cfg_if::cfg_if! {
    if #[cfg(unix)] { fn unix_only() {} } else { fn other() {} }
}
lazy_static! { static ref TABLE: u32 = 1; }
fn f() { let v = vec![1, 2]; my_items! { fn local_macro_fn() {} } }
";
    let map = l1(src);
    let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
    // `static ref` is in the grammar, so a `lazy_static!` table is a real definition; `cfg_if!`
    // is not Rust and the macro inside `f` is in expression position.
    assert_eq!(names, vec!["TABLE", "f"], "{names:?}");
}

#[test]
fn functions_nested_in_function_bodies_are_symbols_but_locals_are_not() {
    let src = "\
fn outer() {
    let x = 1;
    const LOCAL: u32 = 2;
    fn fixture() -> u32 { 3 }
}
";
    let map = l1(src);
    let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"fixture"), "{names:?}");
    assert!(!names.contains(&"x") && !names.contains(&"LOCAL"), "{names:?}");
    let fixture = map.symbols.iter().find(|s| s.name == "fixture").unwrap();
    assert_eq!(
        (fixture.start_row, fixture.start_col, fixture.kind),
        (3, 4, SymbolKind::Function)
    );
}

#[test]
fn bodiless_trait_methods_are_symbols() {
    let src = "\
pub trait Function {
    fn param(&self, index: usize) -> Option<u32>;
    fn provided(&self) {}
}
extern \"C\" { fn c_entry(x: i32) -> i32; }
";
    let map = l1(src);
    let param = map.symbols.iter().find(|s| s.name == "param").expect("param symbol");
    assert_eq!(param.kind, SymbolKind::Function);
    assert_eq!(
        param.signature.as_deref(),
        Some("fn param(&self, index: usize) -> Option<u32>")
    );
    assert_eq!((param.start_row, param.start_col), (1, 4));
    let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
    for want in ["Function", "provided", "c_entry"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
}
