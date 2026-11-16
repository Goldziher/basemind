//! Rust extraction cases a `syn`-based gold extractor found missing from the code map.

use basemind::extract::FileMapL1;
use basemind::extract::l1::extract_l1;
use basemind::extract::l2::extract_l2;

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
fn turbofish_calls_are_recorded() {
    let src = "fn t() { let n = s.parse::<u32>(); let v = collect::<Vec<u8>>(); let w = a::b::make::<T>(); }\n";
    let calls = callees(src);
    for want in ["parse", "collect", "make"] {
        assert!(calls.iter().any(|c| c == want), "{want} missing from {calls:?}");
    }
}
