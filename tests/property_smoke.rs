use basemind::extract::SymbolKind;
use basemind::index::keys::{
    call_by_callee, import_by_module, parse_call_by_callee, parse_import_by_module,
    parse_symbol_by_name, symbol_by_name,
};
use basemind::path::RelPath;
use proptest::prelude::*;

proptest! {
    /// Round-trip `RelPath` through JSON (both the UTF-8 string and the `{"bytes":[...]}`
    /// discriminated-object forms) and through msgpack.
    #[test]
    fn relpath_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let original = RelPath::from(bytes.as_slice());

        // JSON round-trip
        let json = serde_json::to_string(&original)
            .expect("serde_json::to_string should not fail for RelPath");
        let back: RelPath = serde_json::from_str(&json)
            .expect("serde_json::from_str should not fail for RelPath");
        prop_assert_eq!(
            original.as_bytes(),
            back.as_bytes(),
            "JSON round-trip failed for bytes={:?}",
            bytes
        );

        // msgpack round-trip
        let packed = rmp_serde::to_vec_named(&original)
            .expect("rmp_serde::to_vec_named should not fail for RelPath");
        let back_mp: RelPath = rmp_serde::from_slice(&packed)
            .expect("rmp_serde::from_slice should not fail for RelPath");
        prop_assert_eq!(
            original.as_bytes(),
            back_mp.as_bytes(),
            "msgpack round-trip failed for bytes={:?}",
            bytes
        );
    }

    /// Round-trip three Fjall key encoders: `symbol_by_name`, `call_by_callee`,
    /// `import_by_module`. Uses ASCII-safe names (the encoders require valid UTF-8
    /// for the name component) and arbitrary byte sequences for the path to exercise
    /// the non-UTF-8 `RelPath` path.
    #[test]
    fn keys_roundtrip(
        name in "[a-zA-Z0-9_]{1,64}",
        rel_bytes in prop::collection::vec(any::<u8>(), 1..128),
        start_byte in any::<u32>(),
    ) {
        let rel = RelPath::from(rel_bytes.as_slice());

        // symbol_by_name
        let key = symbol_by_name(&name, SymbolKind::Function, &rel, start_byte);
        let (decoded_name, decoded_kind, decoded_rel, decoded_start) =
            parse_symbol_by_name(&key).expect("parse_symbol_by_name failed");
        prop_assert_eq!(&decoded_name, &name);
        prop_assert_eq!(decoded_kind, SymbolKind::Function);
        prop_assert_eq!(decoded_rel.as_bytes(), rel.as_bytes());
        prop_assert_eq!(decoded_start, start_byte);

        // call_by_callee
        let key = call_by_callee(&name, &rel, start_byte);
        let (decoded_callee, decoded_rel2, decoded_start2) =
            parse_call_by_callee(&key).expect("parse_call_by_callee failed");
        prop_assert_eq!(&decoded_callee, &name);
        prop_assert_eq!(decoded_rel2.as_bytes(), rel.as_bytes());
        prop_assert_eq!(decoded_start2, start_byte);

        // import_by_module
        let key = import_by_module(&name, &rel, start_byte);
        let (decoded_module, decoded_rel3, decoded_start3) =
            parse_import_by_module(&key).expect("parse_import_by_module failed");
        prop_assert_eq!(&decoded_module, &name);
        prop_assert_eq!(decoded_rel3.as_bytes(), rel.as_bytes());
        prop_assert_eq!(decoded_start3, start_byte);
    }
}
