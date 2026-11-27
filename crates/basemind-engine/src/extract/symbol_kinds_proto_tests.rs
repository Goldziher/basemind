//! Protocol Buffers and Starlark outlines and imports: every declaration kind each query covers,
//! with the right kind, and nothing from inside a Starlark function body. Fixtures live beside the
//! other languages' under `tests/fixtures/symbol_kinds/`.

use super::SymbolKind::*;
use super::l1::extract_l1;
use super::symbol_kinds_tests::{outline, signature_of, symbols};

const PROTO: &str = include_str!("../../../../tests/fixtures/symbol_kinds/billing.proto");
const STARLARK: &str = include_str!("../../../../tests/fixtures/symbol_kinds/targets.bzl");

/// The `module` of every import of `src`, in source order.
fn import_modules(lang: &'static str, src: &str) -> Vec<Option<String>> {
    let map = extract_l1(lang, src.as_bytes()).expect("extract l1");
    map.imports.into_iter().map(|import| import.module).collect()
}

#[test]
fn proto_outlines_every_declaration_kind_including_nested_ones() {
    let (got, _) = outline("proto", PROTO);
    let want = [
        ("Billing", Interface),
        ("Create", Method),
        ("Invoice", Struct),
        ("K_A", EnumVariant),
        ("K_UNSET", EnumVariant),
        ("Kind", Enum),
        ("Line", Struct),
        ("PAID", EnumVariant),
        ("STATUS_UNSPECIFIED", EnumVariant),
        ("Status", Enum),
        ("Watch", Method),
        ("acme.billing.v1", Namespace),
        ("google.protobuf.MessageOptions", Impl),
        ("java_package", Variable),
        ("payment", Type),
    ];
    assert_eq!(got, symbols(&want));
}

#[test]
fn proto_signatures_are_the_declaration_header() {
    let cases = [
        ("acme.billing.v1", Namespace, "package acme.billing.v1"),
        ("Invoice", Struct, "message Invoice"),
        ("Kind", Enum, "enum Kind"),
        ("K_A", EnumVariant, "K_A = 1"),
        ("payment", Type, "oneof payment"),
        ("Billing", Interface, "service Billing"),
        ("Create", Method, "rpc Create(Invoice) returns (Invoice)"),
        ("Watch", Method, "rpc Watch(stream Invoice) returns (stream Status)"),
        (
            "google.protobuf.MessageOptions",
            Impl,
            "extend google.protobuf.MessageOptions",
        ),
        ("java_package", Variable, "option = \"com.acme\""),
    ];
    for (name, kind, signature) in cases {
        assert_eq!(
            signature_of("proto", PROTO, name),
            (kind, Some(signature.to_string())),
            "{name}"
        );
    }
}

#[test]
fn proto_ignores_options_inside_declarations() {
    let src =
        "syntax = \"proto3\";\nmessage M {\n  option deprecated = true;\n  string a = 1 [deprecated = true];\n}\n";
    let (got, _) = outline("proto", src);
    assert_eq!(got, symbols(&[("M", Struct)]));
}

#[test]
fn proto_imports_carry_the_quoted_path_for_every_import_form() {
    let got = import_modules("proto", PROTO);
    let want = ["google/protobuf/timestamp.proto", "acme/common.proto", "acme/w.proto"];
    assert_eq!(got, want.map(|path| Some(path.to_string())));
}

#[test]
fn starlark_outlines_functions_variables_and_rule_targets() {
    let (got, _) = outline("starlark", STARLARK);
    let want = [
        ("DEPS", Variable),
        ("_private", Variable),
        ("assigned", Class),
        ("lib", Class),
        ("macro", Function),
        ("tool", Class),
        ("x", Variable),
    ];
    assert_eq!(got, symbols(&want));
}

#[test]
fn starlark_target_signature_is_the_rule_name() {
    assert_eq!(
        signature_of("starlark", STARLARK, "lib"),
        (Class, Some("py_library".to_string()))
    );
    assert_eq!(
        signature_of("starlark", STARLARK, "assigned"),
        (Class, Some("my_rule".to_string()))
    );
    assert_eq!(
        signature_of("starlark", STARLARK, "tool"),
        (Class, Some("cc_binary".to_string()))
    );
}

#[test]
fn starlark_ignores_calls_without_a_name_and_calls_in_a_body() {
    let src = "exports_files([\"a\"])\npackage(default_visibility = [\"//visibility:public\"])\ndef f():\n    r(name = \"in_body\")\n";
    let (got, _) = outline("starlark", src);
    assert_eq!(got, symbols(&[("f", Function)]));
}

#[test]
fn starlark_loads_are_imports_by_label() {
    let got = import_modules("starlark", STARLARK);
    let want = ["//tools:defs.bzl", "@rules_python//python:defs.bzl", ":local.bzl"];
    assert_eq!(got, want.map(|label| Some(label.to_string())));
}

#[test]
fn starlark_only_load_calls_are_imports() {
    assert_eq!(
        import_modules("starlark", "other(\"//a:b.bzl\")\n"),
        Vec::<Option<String>>::new()
    );
}
