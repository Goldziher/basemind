use basemind::extract::SymbolKind;
use basemind::index::keys::{
    call_by_callee_hit, content_key, impl_by_path, impl_by_trait, parse_call_by_callee, parse_impl_by_path,
    parse_impl_by_trait, parse_symbol_by_name, symbol_by_name_hit,
};
use basemind::path::RelPath;
use proptest::prelude::*;

proptest! {
    /// Round-trip `RelPath` through JSON (both the UTF-8 string and the `{"bytes":[...]}`
    /// discriminated-object forms) and through msgpack.
    #[test]
    fn relpath_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let original = RelPath::from(bytes.as_slice());

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

    /// Round-trip four Fjall key encoders: the `symbol_by_name` and `call_by_callee` hit keys,
    /// `impl_by_trait`, `impl_by_path`. Uses ASCII-safe names (the
    /// encoders require valid UTF-8 for the name components) and arbitrary byte sequences
    /// for the path to exercise the non-UTF-8 `RelPath` path.
    #[test]
    fn keys_roundtrip(
        name in "[a-zA-Z0-9_]{1,64}",
        impl_type in "[a-zA-Z0-9_]{1,64}",
        rel_bytes in prop::collection::vec(any::<u8>(), 1..128),
        start_byte in any::<u32>(),
    ) {
        let rel = RelPath::from(rel_bytes.as_slice());
        let content = content_key(&name, "rust", 1);

        let key = symbol_by_name_hit(&name, SymbolKind::Function, &content, start_byte, &rel)
            .expect("symbol_by_name: name is ≤64 chars so encoding cannot fail");
        let (decoded_name, decoded_kind, decoded_rel, decoded_start) =
            parse_symbol_by_name(&key).expect("parse_symbol_by_name failed");
        prop_assert_eq!(&decoded_name, &name);
        prop_assert_eq!(decoded_kind, SymbolKind::Function);
        prop_assert_eq!(decoded_rel.as_bytes(), rel.as_bytes());
        prop_assert_eq!(decoded_start, start_byte);

        let key = call_by_callee_hit(&name, &content, start_byte, &rel)
            .expect("call_by_callee: name is ≤64 chars so encoding cannot fail");
        let (decoded_callee, decoded_rel2, decoded_start2) =
            parse_call_by_callee(&key).expect("parse_call_by_callee failed");
        prop_assert_eq!(&decoded_callee, &name);
        prop_assert_eq!(decoded_rel2.as_bytes(), rel.as_bytes());
        prop_assert_eq!(decoded_start2, start_byte);

        let key = impl_by_trait(&name, &impl_type, &rel, start_byte)
            .expect("impl_by_trait: name is ≤64 chars so encoding cannot fail");
        let (decoded_trait, decoded_impl_type, decoded_rel4, decoded_start4) =
            parse_impl_by_trait(&key).expect("parse_impl_by_trait failed");
        prop_assert_eq!(&decoded_trait, &name);
        prop_assert_eq!(&decoded_impl_type, &impl_type);
        prop_assert_eq!(decoded_rel4.as_bytes(), rel.as_bytes());
        prop_assert_eq!(decoded_start4, start_byte);

        let key = impl_by_path(&rel, &name, &impl_type, start_byte)
            .expect("impl_by_path: name is ≤64 chars so encoding cannot fail");
        let (decoded_rel5, decoded_trait2, decoded_impl_type2, decoded_start5) =
            parse_impl_by_path(&key).expect("parse_impl_by_path failed");
        prop_assert_eq!(decoded_rel5.as_bytes(), rel.as_bytes());
        prop_assert_eq!(&decoded_trait2, &name);
        prop_assert_eq!(&decoded_impl_type2, &impl_type);
        prop_assert_eq!(decoded_start5, start_byte);
    }
}
