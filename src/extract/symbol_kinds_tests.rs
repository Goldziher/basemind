//! Each language's outline holds every declaration kind its query covers, with the right kind, and
//! a member-expression or generic base is an implementation. One fixture per language under
//! `tests/fixtures/symbol_kinds/` holds the forms the queries used to miss beside ordinary ones, so
//! an exact outline proves both that the new forms appear and that the old ones are unchanged.

use super::SymbolKind::{self, *};
use super::l1::extract_l1;

type Symbols = Vec<(String, SymbolKind)>;
type Impls = Vec<(String, String)>;

/// Sorted by name, then kind, so the assertions do not depend on match order.
fn sorted_symbols(mut pairs: Symbols) -> Symbols {
    pairs.sort_by_cached_key(|(name, kind)| (name.clone(), format!("{kind:?}")));
    pairs
}

/// The outline of `src` as `(name, kind)` pairs and its implementations as `(implementor, parent)`
/// pairs, both sorted.
fn outline(lang: &'static str, src: &str) -> (Symbols, Impls) {
    let map = extract_l1(lang, src.as_bytes()).expect("extract l1");
    let symbols = map.symbols.into_iter().map(|s| (s.name, s.kind)).collect();
    let mut impls: Impls = map
        .implementations
        .into_iter()
        .map(|i| (i.impl_type, i.trait_name))
        .collect();
    impls.sort();
    (sorted_symbols(symbols), impls)
}

fn symbols(pairs: &[(&str, SymbolKind)]) -> Symbols {
    sorted_symbols(pairs.iter().map(|(name, kind)| (name.to_string(), *kind)).collect())
}

fn impls(pairs: &[(&str, &str)]) -> Impls {
    let mut out: Impls = pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    out.sort();
    out
}

/// The TypeScript fixture's outline, shared by `.ts` and `.tsx` (the TSX fixture extends it).
const TS_SYMBOLS: &[(&str, SymbolKind)] = &[
    ("Base", Class),
    ("Named", Interface),
    ("Panel", Class),
    ("React", Const),
    ("Repo", Interface),
    ("Store", Interface),
    ("find", Method),
    ("find", Method),
    ("get", Method),
    ("greet", Function),
    ("handleClick", Method),
    ("label", Field),
    ("label", Field),
    ("legacy", Variable),
    ("legacyHandler", Function),
    ("set", Method),
    ("size", Field),
    ("size", Field),
    ("total", Getter),
    ("total", Setter),
];

const TS_IMPLS: &[(&str, &str)] = &[
    ("Base", "Named"),
    ("Base", "Repo"),
    ("Panel", "Component"),
    ("Store", "Repo"),
];

#[test]
fn typescript_outlines_abstract_classes_members_ambient_functions_and_var() {
    let (got, got_impls) = outline("typescript", include_str!("../../tests/fixtures/symbol_kinds/kinds.ts"));
    assert_eq!(got, symbols(TS_SYMBOLS));
    assert_eq!(got_impls, impls(TS_IMPLS));
}

#[test]
fn tsx_outlines_abstract_classes_members_ambient_functions_and_var() {
    let (got, got_impls) = outline("tsx", include_str!("../../tests/fixtures/symbol_kinds/kinds.tsx"));
    let mut want = TS_SYMBOLS.to_vec();
    want.extend([("Button", Class), ("onPress", Method)]);
    let mut want_impls = TS_IMPLS.to_vec();
    want_impls.push(("Button", "Component"));
    assert_eq!(got, symbols(&want));
    assert_eq!(got_impls, impls(&want_impls));
}

#[test]
fn javascript_outlines_var_class_fields_and_commonjs_exports() {
    let (got, got_impls) = outline("javascript", include_str!("../../tests/fixtures/symbol_kinds/kinds.js"));
    let want = [
        ("Panel", Class),
        ("VERSION", Const),
        ("build", Function),
        ("count", Field),
        ("get", Method),
        ("handleClick", Method),
        ("helper", Function),
        ("legacy", Variable),
        ("legacyHandler", Function),
        ("module.exports", Function),
        ("set", Method),
        ("total", Getter),
        ("total", Setter),
    ];
    assert_eq!(got, symbols(&want));
    assert_eq!(got_impls, impls(&[("Panel", "Component")]));
}

#[test]
fn java_outlines_enums_records_annotation_types_constructors_and_fields() {
    let (got, got_impls) = outline("java", include_str!("../../tests/fixtures/symbol_kinds/Kinds.java"));
    let want = [
        ("Account", Class),
        ("Account", Constructor),
        ("Color", Enum),
        ("Limits", Interface),
        ("MAX_ITEMS", Const),
        ("Marker", Interface),
        ("PREFIX", Const),
        ("Point", Class),
        ("Point", Constructor),
        ("VERSION", Const),
        ("currency", Field),
        ("owner", Field),
        ("value", Method),
    ];
    assert_eq!(got, symbols(&want));
    assert_eq!(got_impls, Impls::new());
}

#[test]
fn python_outlines_annotated_fields_and_type_aliases() {
    let (got, got_impls) = outline("python", include_str!("../../tests/fixtures/symbol_kinds/kinds.py"));
    let want = [
        ("Box", Class),
        ("Holder", Class),
        ("Pair", Type),
        ("T", Const),
        ("User", Class),
        ("UserId", Type),
        ("age", Const),
        ("build", Function),
        ("item", Field),
        ("name", Field),
        ("registry", Variable),
    ];
    assert_eq!(got, symbols(&want));
    assert_eq!(got_impls, impls(&[("Box", "Generic"), ("Holder", "Generic")]));
}

#[test]
fn go_outlines_type_aliases_and_interface_methods() {
    let (got, got_impls) = outline("go", include_str!("../../tests/fixtures/symbol_kinds/kinds.go"));
    let want = [
        ("Closer", Type),
        ("Get", Method),
        ("ID", Type),
        ("Name", Type),
        ("Put", Method),
        ("Record", Type),
        ("Shut", Method),
        ("Store", Type),
        ("Use", Function),
    ];
    assert_eq!(got, symbols(&want));
    assert_eq!(got_impls, Impls::new());
}

/// The `(kind, signature)` of the symbol `name` in `src`.
fn signature_of(lang: &'static str, src: &str, name: &str) -> (SymbolKind, Option<String>) {
    let map = extract_l1(lang, src.as_bytes()).expect("extract l1");
    let sym = map
        .symbols
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no symbol named {name}"));
    (sym.kind, sym.signature)
}

#[test]
fn terraform_outlines_blocks_and_locals_but_not_nested_blocks() {
    let (got, got_impls) = outline("terraform", include_str!("../../tests/fixtures/symbol_kinds/main.tf"));
    let want = [
        ("assume", Class),
        ("aws", Namespace),
        ("bucket_arn", Const),
        ("logs", Class),
        ("network", Module),
        ("prefix", Variable),
        ("region", Variable),
        ("tags", Variable),
        ("terraform", Namespace),
    ];
    assert_eq!(got, symbols(&want));
    assert_eq!(got_impls, Impls::new());
}

#[test]
fn hcl_outlines_terragrunt_include_dependency_terraform_locals_and_inputs() {
    let (got, _) = outline("hcl", include_str!("../../tests/fixtures/symbol_kinds/terragrunt.hcl"));
    let want = [
        ("env", Variable),
        ("include", Module),
        ("inputs", Variable),
        ("root", Module),
        ("terraform", Namespace),
        ("vpc", Module),
    ];
    assert_eq!(got, symbols(&want));
}

#[test]
fn terraform_signatures_are_the_block_header() {
    let src = include_str!("../../tests/fixtures/symbol_kinds/main.tf");
    let cases = [
        ("logs", Class, "resource \"aws_s3_bucket\" \"logs\""),
        ("assume", Class, "data \"aws_iam_policy_document\" \"assume\""),
        ("region", Variable, "variable \"region\""),
        ("bucket_arn", Const, "output \"bucket_arn\""),
        ("network", Module, "module \"network\""),
        ("aws", Namespace, "provider \"aws\""),
        ("terraform", Namespace, "terraform"),
    ];
    for (name, kind, signature) in cases {
        assert_eq!(
            signature_of("terraform", src, name),
            (kind, Some(signature.to_string())),
            "{name}"
        );
    }
}
