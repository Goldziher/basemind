//! A source file that is not UTF-8 (Latin-1 here) is indexed: outlined, its symbols searchable, at
//! the offsets of its own bytes. Each byte of an invalid sequence reads as `_`.

use basemind::config::ConfigV1;
use basemind::extract::SymbolKind;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING, init_isolated_cache};

/// `Café.java` saved as Latin-1: `é` is the single byte `0xE9`, `è` is `0xE8`, `û` is `0xFB`.
const LATIN1_JAVA: &[u8] = b"/* Menu du caf\xe9 : cr\xe8me br\xfbl\xe9e */\n\
public class Caf\xe9 {\n    public String cr\xe8me() {\n        return \"cr\xe8me br\xfbl\xe9e\";\n    }\n}\n";

#[test]
fn a_latin1_java_file_is_outlined_at_its_own_offsets() {
    init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("Cafe.java"), LATIN1_JAVA).expect("write fixture");

    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let report = scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    assert_eq!(report.stats.updated, 1, "the file is indexed: {:?}", report.stats);

    let entry = store.lookup("Cafe.java").expect("indexed");
    assert_eq!(
        (entry.language.as_str(), entry.size_bytes),
        ("java", LATIN1_JAVA.len() as u64)
    );

    let l1 = basemind::query::file_outline(&store, "Cafe.java").expect("outline");
    let mut symbols: Vec<(&str, SymbolKind, u32, u32, u32)> = l1
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.kind, s.start_byte, s.end_byte, s.start_row))
        .collect();
    symbols.sort_by_key(|s| s.2);
    assert_eq!(
        symbols,
        [
            ("Caf_", SymbolKind::Class, 34, 120, 1),
            ("cr_me", SymbolKind::Method, 58, 118, 2),
        ]
    );
    assert_eq!(&LATIN1_JAVA[58..71], b"public String");

    let hits = basemind::query::search_symbols(&store, "cr_me", None).expect("search");
    assert_eq!(hits.len(), 1, "{hits:?}");
}
