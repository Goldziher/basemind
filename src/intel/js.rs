//! JavaScript / TypeScript code-intelligence via oxc.
//!
//! oxc parses JS/TS with its own parser (no tree-sitter grammar needed) and hands us a fully
//! resolved scope + symbol + reference model (`oxc_semantic::Scoping`). This module turns that
//! into basemind's shape: per-file **resolved references** (each use linked to its definition by
//! byte span, shadowing already applied) plus the **import/export** edges the scanner's
//! cross-file second pass stitches through `oxc_resolver`.
//!
//! This is the single-file half of Phase 2 — pure analysis of one source string, independent of
//! the index/blob layer, so it is unit-testable directly against oxc's parser.

use std::path::Path;
use std::sync::Arc;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::module_record::{ExportExportName, ImportImportName};

/// A resolved intra-file reference: a use of a symbol linked to the definition it binds to, both
/// as byte spans. oxc has already applied scope/shadowing resolution, so `def_start` is the
/// *correct* binding — not a name match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRef {
    /// `Arc<str>` so the symbol name is promoted once per symbol and shared (atomic bump)
    /// across all of its references, rather than heap-cloned per reference on the scan path.
    pub name: Arc<str>,
    pub def_start: u32,
    pub def_end: u32,
    pub use_start: u32,
    pub use_end: u32,
}

/// An import binding introduced in this file: the local name, the module specifier it came from,
/// and the imported name in the source module (`None` for default / namespace imports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsImport {
    /// Local binding name in this file (what references resolve to).
    pub local: String,
    /// Module specifier, e.g. `./bar` or `react`.
    pub specifier: String,
    /// Name exported by the source module: `Some("baz")` for `import { baz }`; `None` for
    /// `import x` (default) and `import * as ns` (namespace).
    pub imported: Option<String>,
    /// True for type-only imports (`import type { Foo }`, `import { type Foo }`). These are
    /// erased at runtime, so the cross-file stitch must not treat them as runtime reference edges.
    pub is_type: bool,
    /// Start byte of the local binding identifier.
    pub local_start: u32,
}

/// A name this module exports (for the cross-file stitch: an importer binds to one of these).
///
/// Covers only direct named exports (`export function foo`, `export const foo`, `export { foo }`).
/// Re-exports (`export { foo } from './mod'`) live in oxc's `indirect_export_entries` and star
/// exports in `star_export_entries` — both are picked up by the cross-file stitch slice, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsExport {
    pub name: String,
    /// Start byte of the exported name identifier in the export clause (not necessarily the
    /// definition site — for `export { foo }` this is the clause `foo`, resolved to a def via
    /// `resolved`).
    pub name_start: u32,
}

/// Result of analyzing a single JS/TS source file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JsAnalysis {
    pub resolved: Vec<ResolvedRef>,
    pub imports: Vec<JsImport>,
    pub exports: Vec<JsExport>,
    /// True when oxc reported any parse diagnostics or bailed (`panicked`) — the analysis is
    /// best-effort in that case (the AST may be partial).
    pub had_errors: bool,
}

/// Resolve the oxc [`SourceType`] for a path (drives JSX/TS/module flags). Returns `None` for
/// extensions oxc doesn't recognize as JS/TS.
pub fn source_type_for_path(path: &Path) -> Option<SourceType> {
    SourceType::from_path(path).ok()
}

/// Analyze a single JS/TS source string: resolved intra-file references + import/export edges.
///
/// The heavy lifting is oxc's: [`Parser`] builds the AST + module record, [`SemanticBuilder`]
/// resolves scopes and links every reference to its binding symbol. We read the resolved model
/// out into owned, span-keyed records that the scanner can persist.
pub fn analyze(source: &str, source_type: SourceType) -> JsAnalysis {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    // `with_build_nodes` is required so we can map a reference's NodeId back to a span below.
    let semantic_ret = SemanticBuilder::new().with_build_nodes(true).build(&parsed.program);
    let semantic = semantic_ret.semantic;
    let scoping = semantic.scoping();
    let nodes = semantic.nodes();

    // Resolved references: for each symbol, its definition span + every use oxc resolved to it.
    let mut resolved = Vec::new();
    for symbol_id in scoping.symbol_ids() {
        let def_span = scoping.symbol_span(symbol_id);
        // Promote the name once per symbol; each reference shares it via an atomic bump.
        let name: Arc<str> = Arc::from(scoping.symbol_name(symbol_id));
        for reference in scoping.get_resolved_references(symbol_id) {
            // A resolved reference's NodeId points at its `IdentifierReference` node, so this
            // span is the use-site identifier token.
            let use_span = nodes.kind(reference.node_id()).span();
            resolved.push(ResolvedRef {
                name: Arc::clone(&name),
                def_start: def_span.start,
                def_end: def_span.end,
                use_start: use_span.start,
                use_end: use_span.end,
            });
        }
    }

    // Import edges from the module record.
    let imports = parsed
        .module_record
        .import_entries
        .iter()
        .map(|entry| JsImport {
            local: entry.local_name.name.as_str().to_string(),
            specifier: entry.module_request.name.as_str().to_string(),
            imported: match &entry.import_name {
                ImportImportName::Name(ns) => Some(ns.name.as_str().to_string()),
                ImportImportName::Default(_) | ImportImportName::NamespaceObject => None,
            },
            is_type: entry.is_type,
            local_start: entry.local_name.span.start,
        })
        .collect();

    // Export edges (best-effort — named exports; default/star handled in the cross-file slice).
    let exports = parsed
        .module_record
        .local_export_entries
        .iter()
        .filter_map(|entry| match &entry.export_name {
            ExportExportName::Name(ns) => Some(JsExport {
                name: ns.name.as_str().to_string(),
                name_start: ns.span.start,
            }),
            _ => None,
        })
        .collect();

    JsAnalysis {
        resolved,
        imports,
        exports,
        had_errors: parsed.panicked || !parsed.diagnostics.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(source: &str) -> JsAnalysis {
        analyze(source, SourceType::ts())
    }

    #[test]
    fn resolves_call_to_its_definition() {
        // `greet(...)` and the `name` return both resolve to their in-file definitions.
        let src = "function greet(name) {\n  return name;\n}\ngreet(\"hi\");\n";
        let a = ts(src);
        assert!(!a.had_errors);
        let greet_def = src.find("function greet").unwrap() + "function ".len();
        let greet_call = src.rfind("greet(").unwrap();
        let call_ref = a
            .resolved
            .iter()
            .find(|r| &*r.name == "greet" && r.use_start as usize == greet_call)
            .expect("greet call must resolve");
        assert_eq!(
            call_ref.def_start as usize, greet_def,
            "greet call binds to the function def"
        );
    }

    #[test]
    fn shadowing_resolves_to_inner_binding() {
        // The `x` returned inside `f` must resolve to the INNER const, not the module-level one.
        let src = "const x = 1;\nfunction f() {\n  const x = 2;\n  return x;\n}\n";
        let a = ts(src);
        let outer_x = src.find("const x = 1").unwrap() + "const ".len();
        let inner_x = src.find("const x = 2").unwrap() + "const ".len();
        let use_x = src.rfind("return x").unwrap() + "return ".len();
        let r = a
            .resolved
            .iter()
            .find(|r| &*r.name == "x" && r.use_start as usize == use_x)
            .expect("inner x use must resolve");
        assert_eq!(r.def_start as usize, inner_x, "must bind to inner x, not outer");
        assert_ne!(r.def_start as usize, outer_x);
    }

    #[test]
    fn extracts_named_and_default_imports() {
        let src = "import def, { foo, bar as baz } from './mod';\nimport * as ns from 'pkg';\nimport type { T } from './t';\n";
        let a = analyze(src, SourceType::ts());
        let by_local = |l: &str| a.imports.iter().find(|i| i.local == l).cloned();

        let foo = by_local("foo").expect("named import foo");
        assert_eq!(foo.specifier, "./mod");
        assert_eq!(foo.imported.as_deref(), Some("foo"));
        assert!(!foo.is_type, "runtime import must have is_type=false");

        let t = by_local("T").expect("type-only import T");
        assert!(t.is_type, "`import type` must set is_type=true");

        let baz = by_local("baz").expect("aliased import baz");
        assert_eq!(
            baz.imported.as_deref(),
            Some("bar"),
            "aliased import keeps the source name"
        );

        let def = by_local("def").expect("default import");
        assert_eq!(def.imported, None, "default import has no source name");

        let ns = by_local("ns").expect("namespace import");
        assert_eq!(ns.imported, None, "namespace import has no source name");
    }

    #[test]
    fn extracts_named_exports() {
        let src = "export function alpha() {}\nexport const beta = 1;\n";
        let a = analyze(src, SourceType::mjs());
        let mut names: Vec<&str> = a.exports.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["alpha", "beta"], "exactly alpha and beta must be exported");
    }

    #[test]
    fn free_identifier_is_not_resolved() {
        // `fetch` is a global — oxc leaves it unresolved (no in-file symbol), so it never appears
        // as a resolved reference. This is the cross-file-candidate signal.
        let src = "function f() {\n  return fetch('/x');\n}\n";
        let a = ts(src);
        assert!(
            !a.resolved.iter().any(|r| &*r.name == "fetch"),
            "global `fetch` must not resolve to an in-file def"
        );
    }
}
