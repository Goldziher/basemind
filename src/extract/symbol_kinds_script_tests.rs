//! SQL, shell and SCSS outlines: the declarations each query covers, with the right kind, and
//! nothing from inside a function, mixin or rule body. Fixtures live beside the other languages'
//! under `tests/fixtures/symbol_kinds/`.

use super::SymbolKind::*;
use super::symbol_kinds_tests::{outline, signature_of, symbols};

#[test]
fn sql_outlines_create_statements_and_nothing_else() {
    let (got, _) = outline("sql", include_str!("../../tests/fixtures/symbol_kinds/schema.sql"));
    let want = [
        ("active_users", Type),
        ("add", Function),
        ("app", Namespace),
        ("audit_users", Function),
        ("idx_users_name", Variable),
        ("mood", Type),
        ("purge", Function),
        ("user_counts", Type),
        ("user_ids", Variable),
        ("users", Struct),
    ];
    assert_eq!(got, symbols(&want));
}

#[test]
fn sql_signatures_are_the_statement_header() {
    let src = include_str!("../../tests/fixtures/symbol_kinds/schema.sql");
    let cases = [
        ("users", Struct, "CREATE TABLE IF NOT EXISTS public.users"),
        ("mood", Type, "CREATE TYPE mood"),
        ("active_users", Type, "CREATE VIEW active_users"),
        (
            "add",
            Function,
            "CREATE OR REPLACE FUNCTION public.add(a int, b int) RETURNS int",
        ),
        ("purge", Function, "CREATE PROCEDURE purge()"),
        (
            "idx_users_name",
            Variable,
            "CREATE UNIQUE INDEX idx_users_name ON users",
        ),
    ];
    for (name, kind, signature) in cases {
        assert_eq!(
            signature_of("sql", src, name),
            (kind, Some(signature.to_string())),
            "{name}"
        );
    }
}

#[test]
fn sql_ignores_unnamed_indexes_and_queries() {
    let (got, _) = outline("sql", "CREATE INDEX ON users (id);\nSELECT 1;\nDROP TABLE old;\n");
    assert_eq!(got, symbols(&[]));
}

#[test]
fn bash_outlines_functions_and_top_level_variables() {
    let (got, _) = outline("bash", include_str!("../../tests/fixtures/symbol_kinds/deploy.sh"));
    let want = [
        ("A", Variable),
        ("B", Variable),
        ("MODE", Variable),
        ("REGION", Variable),
        ("ROOT", Variable),
        ("VERSION", Variable),
        ("build_all", Function),
        ("ci_only", Function),
        ("deploy", Function),
        ("log", Function),
    ];
    assert_eq!(got, symbols(&want));
}

#[test]
fn bash_signatures_are_the_function_header_and_the_assignment() {
    let src = include_str!("../../tests/fixtures/symbol_kinds/deploy.sh");
    let cases = [
        ("log", Function, "log()"),
        ("deploy", Function, "function deploy"),
        ("build_all", Function, "function build_all()"),
        ("VERSION", Variable, "=1.2"),
    ];
    for (name, kind, signature) in cases {
        assert_eq!(
            signature_of("bash", src, name),
            (kind, Some(signature.to_string())),
            "{name}"
        );
    }
}

#[test]
fn scss_outlines_variables_mixins_functions_and_placeholders() {
    let (got, _) = outline("scss", include_str!("../../tests/fixtures/symbol_kinds/theme.scss"));
    let want = [
        ("$gap", Variable),
        ("$primary", Variable),
        ("%card", Class),
        ("button", Macro),
        ("rem", Function),
    ];
    assert_eq!(got, symbols(&want));
}

#[test]
fn scss_signatures_are_the_header_up_to_the_brace() {
    let src = include_str!("../../tests/fixtures/symbol_kinds/theme.scss");
    let cases = [
        ("button", Macro, "@mixin button($color: red)"),
        ("rem", Function, "@function rem($px)"),
        ("%card", Class, "%card"),
        ("$primary", Variable, "$primary: #0a84ff"),
    ];
    for (name, kind, signature) in cases {
        assert_eq!(
            signature_of("scss", src, name),
            (kind, Some(signature.to_string())),
            "{name}"
        );
    }
}
