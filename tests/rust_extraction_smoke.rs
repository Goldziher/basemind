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

#[test]
fn an_epoch_refresh_re_extracts_the_calls_of_an_old_blob_with_eager_l2_off() {
    use basemind::scanner::{EmbedMode, ScanSource, scan};
    use basemind::store::{FileEntry, Store, VIEW_WORKING};

    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let src = "// epoch-l2-rust\nfn t(s: &str) -> u32 { s.parse::<u32>().unwrap() }\n";
    std::fs::write(root.join("lib.rs"), src).unwrap();
    let mut cfg = basemind::config::ConfigV1::with_defaults();
    cfg.scan.eager_l2 = false;
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    let entry = store.lookup("lib.rs").expect("lib.rs indexed").clone();

    // The blob an extractor from before the turbofish fix left: L1 and L2, the L2 without `parse`.
    let (mut old_l1, old_l2) = basemind::extract::extract_l1_l2("rust", src.as_bytes(), true).expect("extract");
    let mut old_l2 = old_l2.expect("l2");
    old_l2.calls.retain(|c| c.callee != "parse");
    old_l1.extract_epoch = 1;
    store
        .overwrite_filemap_hex(&entry.hash_hex, &old_l1, Some(&old_l2))
        .unwrap();
    store.upsert(
        "lib.rs",
        FileEntry {
            extract_epoch: 1,
            ..entry.clone()
        },
    );
    store.flush().unwrap();

    let report = scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("rescan");
    assert_eq!(
        report.stats.refreshed_extraction, 1,
        "premise: the stale blob is refreshed"
    );
    let l2 = store
        .read_l2_by_hex(&entry.hash_hex)
        .unwrap()
        .expect("the refreshed blob keeps a calls tier");
    let calls: Vec<&str> = l2.calls.iter().map(|c| c.callee.as_str()).collect();
    assert!(calls.contains(&"parse"), "stale calls were carried over: {calls:?}");
}

/// A chunk sidecar cut from a pre-epoch outline is not reused when the refreshed outline chunks
/// differently: the function inside the macro body gets its own chunk.
#[cfg(feature = "code-search")]
#[test]
fn an_epoch_refresh_re_chunks_a_file_whose_outline_changed() {
    use basemind::chunk::{ChunkOptions, CodeChunkBlob, chunk_file};
    use basemind::scanner::{EmbedMode, ScanSource, scan};
    use basemind::store::{FileEntry, Store, VIEW_WORKING};

    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let src = "// epoch-chunks-rust\ncfg_rt! {\n    pub fn spawn_inside() -> u32 { 1 }\n}\npub fn top() {}\n";
    std::fs::write(root.join("lib.rs"), src).unwrap();
    let mut cfg = basemind::config::ConfigV1::with_defaults();
    cfg.code_search.enabled = true;
    cfg.code_search.embed = false;
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    let entry = store.lookup("lib.rs").expect("lib.rs indexed").clone();
    let has_inside = |blob: &CodeChunkBlob| blob.chunks.iter().any(|c| c.symbol.as_deref() == Some("spawn_inside"));
    let current = store
        .read_chunks_by_hex(&entry.hash_hex)
        .unwrap()
        .expect("chunk sidecar");
    assert!(
        has_inside(&current),
        "premise: the current outline chunks the macro-body fn"
    );

    // What an extractor without macro-body items left: no `spawn_inside` symbol, chunks cut from that.
    let (mut old_l1, old_l2) = basemind::extract::extract_l1_l2("rust", src.as_bytes(), true).expect("extract");
    old_l1.symbols.retain(|s| s.name != "spawn_inside");
    old_l1.extract_epoch = 1;
    let opts = ChunkOptions {
        max_characters: cfg.code_search.max_characters,
        overlap: cfg.code_search.overlap,
    };
    let old_chunks = chunk_file(
        "lib.rs",
        &entry.hash_hex,
        &old_l1,
        old_l2.as_ref(),
        src.as_bytes(),
        opts,
    );
    let stale = CodeChunkBlob {
        chunks: old_chunks,
        ..current
    };
    assert!(
        !has_inside(&stale),
        "premise: the stale sidecar lacks the macro-body fn"
    );
    store.write_chunks_hex(&entry.hash_hex, &stale).unwrap();
    store
        .overwrite_filemap_hex(&entry.hash_hex, &old_l1, old_l2.as_ref())
        .unwrap();
    store.upsert(
        "lib.rs",
        FileEntry {
            extract_epoch: 1,
            ..entry.clone()
        },
    );
    store.flush().unwrap();

    let report = scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("rescan");
    assert_eq!(
        report.stats.refreshed_extraction, 1,
        "premise: the stale blob is refreshed"
    );
    let refreshed = store
        .read_chunks_by_hex(&entry.hash_hex)
        .unwrap()
        .expect("chunk sidecar");
    assert!(has_inside(&refreshed), "the stale sidecar was reused");
}

/// A refresh scan run with code-search off rewrites the outline but leaves the chunk sidecar cut from
/// the old one. Once code-search is on, that sidecar's own epoch marks it stale: it is re-chunked when
/// the outline chunks differently, and re-stamped (not re-embedded) when it chunks the same.
#[cfg(feature = "code-search")]
#[test]
fn a_chunk_sidecar_from_an_older_epoch_is_re_chunked_under_a_current_outline() {
    use basemind::chunk::{ChunkOptions, CodeChunkBlob, chunk_file};
    use basemind::extract::EXTRACT_EPOCH;
    use basemind::scanner::{EmbedMode, ScanSource, scan};
    use basemind::store::{Store, VIEW_WORKING};

    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let src = "// epoch-stamped-chunks-rust\ncfg_rt! {\n    pub fn stamped_inside() -> u32 { 1 }\n}\npub fn top() {}\n";
    std::fs::write(root.join("lib.rs"), src).unwrap();
    let mut cfg = basemind::config::ConfigV1::with_defaults();
    cfg.code_search.enabled = true;
    cfg.code_search.embed = false;
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    let entry = store.lookup("lib.rs").expect("lib.rs indexed").clone();
    assert_eq!(entry.extract_epoch, EXTRACT_EPOCH, "premise: the outline is current");
    let has_inside = |blob: &CodeChunkBlob| {
        blob.chunks
            .iter()
            .any(|c| c.symbol.as_deref() == Some("stamped_inside"))
    };
    let current = store
        .read_chunks_by_hex(&entry.hash_hex)
        .unwrap()
        .expect("chunk sidecar");
    assert_eq!(current.extract_epoch, EXTRACT_EPOCH, "a new sidecar is stamped");
    assert!(
        has_inside(&current),
        "premise: the current outline chunks the macro-body fn"
    );

    let (mut old_l1, old_l2) = basemind::extract::extract_l1_l2("rust", src.as_bytes(), true).expect("extract");
    old_l1.symbols.retain(|s| s.name != "stamped_inside");
    let opts = ChunkOptions {
        max_characters: cfg.code_search.max_characters,
        overlap: cfg.code_search.overlap,
    };
    let old_chunks = chunk_file(
        "lib.rs",
        &entry.hash_hex,
        &old_l1,
        old_l2.as_ref(),
        src.as_bytes(),
        opts,
    );
    let stale = CodeChunkBlob {
        chunks: old_chunks,
        extract_epoch: 0,
        ..current.clone()
    };
    assert!(
        !has_inside(&stale),
        "premise: the stale sidecar lacks the macro-body fn"
    );
    store.write_chunks_hex(&entry.hash_hex, &stale).unwrap();

    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("rescan");
    let rechunked = store
        .read_chunks_by_hex(&entry.hash_hex)
        .unwrap()
        .expect("chunk sidecar");
    assert!(has_inside(&rechunked), "the pre-epoch sidecar was trusted");
    assert_eq!(rechunked.extract_epoch, EXTRACT_EPOCH);

    let unstamped = CodeChunkBlob {
        extract_epoch: 0,
        ..rechunked.clone()
    };
    store.write_chunks_hex(&entry.hash_hex, &unstamped).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("rescan");
    let restamped = store
        .read_chunks_by_hex(&entry.hash_hex)
        .unwrap()
        .expect("chunk sidecar");
    assert_eq!(restamped.chunks, rechunked.chunks, "same chunks are kept");
    assert_eq!(
        restamped.extract_epoch, EXTRACT_EPOCH,
        "a sidecar that re-chunks the same is stamped, so the next scan skips it"
    );
}

fn count(calls: &[String], name: &str) -> usize {
    calls.iter().filter(|c| *c == name).count()
}

#[test]
fn calls_in_a_parsed_macro_body_come_from_the_calls_query_once() {
    let src = "\
cfg_rt! {
    #[cfg_attr(docsrs, doc(cfg(all(feature = \"rt\", not(loom)))))]
    #[derive(Debug)]
    #[repr(C)]
    pub enum Kind { CurrentThread(CT), Multi(MT) }

    pub fn spawn<F: FnOnce() -> u32>(f: F, s: &str) -> u32 {
        if let Ok(v) = s.parse::<u32>() { helper(v); }
        matches!(f, Some(_));
        x.method(1)
    }
}
";
    let calls = callees(src);
    for want in ["helper", "method", "parse", "cfg_rt", "matches"] {
        assert_eq!(count(&calls, want), 1, "{want} once in {calls:?}");
    }
    for bad in [
        "cfg_attr",
        "doc",
        "cfg",
        "all",
        "not",
        "derive",
        "repr",
        "CurrentThread",
        "Multi",
        "FnOnce",
        "spawn",
        "Ok",
        "Some",
    ] {
        assert_eq!(count(&calls, bad), 0, "{bad} must not be a call: {calls:?}");
    }
    let l2 = extract_l2("rust", src.as_bytes()).expect("extract l2");
    let helper = l2.calls.iter().find(|c| c.callee == "helper").expect("helper");
    assert_eq!((helper.start_row, helper.start_col), (7, 42));
    assert_eq!(&src[helper.start_byte as usize..helper.end_byte as usize], "helper(v)");
}

#[test]
fn lexical_calls_skip_attributes_fn_bounds_bindings_and_patterns() {
    let src = "\
cfg_if::cfg_if! {
    if #[cfg(all(unix, not(loom)))] {
        #![allow(dead_code)]
        fn f() { g() }
    }
}
fn t() {
    assert!(f(a, (b)));
    assert!(v[0] == 1);
    assert!(matches!(r, Some(Ok(_)) if guard(r)));
    takes!(Box<dyn FnMut(u8)>, Fn(u8), FnOnce());
    run!(if let Ok(v) = parse_it(s) { use_it(v) });
    assert!(cfg!(any(unix, not(windows))) || std::env::var(key(1)).is_ok());
}
";
    let calls = callees(src);
    for want in ["g", "f", "guard", "parse_it", "use_it", "key"] {
        assert_eq!(count(&calls, want), 1, "{want} once in {calls:?}");
    }
    for bad in [
        "cfg", "all", "any", "not", "allow", "a", "b", "v", "Some", "Ok", "FnMut", "Fn", "FnOnce",
    ] {
        assert_eq!(count(&calls, bad), 0, "{bad} must not be a call: {calls:?}");
    }
}

#[test]
fn turbofish_is_recorded_once() {
    let calls = callees("fn t() { let n = s.parse::<u32>(); }\n");
    assert_eq!(count(&calls, "parse"), 1, "{calls:?}");
}

#[test]
fn macro_body_positions_on_the_opening_line_carry_its_column() {
    let one = l1("m! { pub fn one() {} }\n");
    let sym = one.symbols.iter().find(|s| s.name == "one").expect("one");
    assert_eq!((sym.start_byte, sym.start_row, sym.start_col), (5, 0, 5));

    let nested = l1("a! { b! { fn c() {} } }\n");
    let c = nested.symbols.iter().find(|s| s.name == "c").expect("c");
    assert_eq!((c.start_byte, c.start_row, c.start_col), (10, 0, 10));
}

#[test]
fn trait_impls_inside_macro_bodies_are_implementations() {
    let map = l1("m! {\n    impl Tr for S {}\n}\n");
    let pairs = impl_pairs(&map);
    assert_eq!(pairs, vec![("Tr", "S")]);
    let imp = &map.implementations[0];
    assert_eq!((imp.start_byte, imp.start_row, imp.start_col), (9, 1, 4));
}

#[test]
fn macro_bodies_open_three_layers_deep_and_no_further() {
    let map = l1("a! { b! { c! { fn in3() {} d! { fn deep4() {} } } } }\n");
    let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"in3"), "{names:?}");
    assert!(!names.contains(&"deep4"), "{names:?}");
}

#[test]
fn negative_impls_are_not_implementations_and_path_generic_impls_are_symbols() {
    let map = l1("impl !Send for X {}\nimpl Tr for a::B<T> {}\n");
    assert_eq!(impl_pairs(&map), vec![("Tr", "B")]);
    let impls: Vec<&str> = map
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Impl)
        .map(|s| s.name.as_str())
        .collect();
    assert!(impls.contains(&"B"), "{impls:?}");
}
