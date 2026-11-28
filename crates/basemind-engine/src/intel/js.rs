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

use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::{Argument, Expression};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::module_record::{ExportExportName, ExportImportName, ImportImportName, ModuleRecord};

use crate::intel::model::{NamedReexport, ReexportEdge};

/// A resolved intra-file reference: a use of a symbol linked to the definition it binds to, both
/// as byte spans. oxc has already applied scope/shadowing resolution, so `def_start` is the
/// *correct* binding — not a name match. The identifier text is intentionally not carried: the
/// scanner only persists the spans (`ResolvedEdge`), and any name is recoverable from the source at
/// `def_start..def_end` on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRef {
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
    /// `import x` (default). A namespace import (`import * as ns`) has no single export site to bind
    /// to, so it produces no binding `JsImport`; its module request is a `module_only` one.
    pub imported: Option<String>,
    /// True for type-only imports (`import type { Foo }`, `import { type Foo }`). These are
    /// erased at runtime, so the cross-file stitch must not treat them as runtime reference edges.
    pub is_type: bool,
    /// Start byte of the local binding identifier, or of the specifier for a module-only import.
    pub local_start: u32,
    /// A module request that binds no name (side-effect import, namespace import, re-export): a file
    /// edge only. `local` is empty and `imported` is `None`.
    pub module_only: bool,
}

/// A name this module exports (for the cross-file stitch: an importer binds to one of these).
///
/// Covers only direct named exports (`export function foo`, `export const foo`, `export { foo }`).
/// Re-exports (`export { foo } from './mod'`, `export * from './mod'`) are [`ReexportEdge`]s in
/// [`JsAnalysis::reexports`].
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
    /// Names forwarded from other modules: named and star re-exports, in source order within each
    /// kind (named first). An `export { x }` of an imported `x` is a named re-export too.
    pub reexports: Vec<ReexportEdge>,
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
    let semantic_ret = SemanticBuilder::new().with_build_nodes(true).build(&parsed.program);
    let semantic = semantic_ret.semantic;
    let scoping = semantic.scoping();
    let nodes = semantic.nodes();

    let mut resolved = Vec::new();
    for symbol_id in scoping.symbol_ids() {
        let def_span = scoping.symbol_span(symbol_id);
        for reference in scoping.get_resolved_references(symbol_id) {
            let use_span = nodes.kind(reference.node_id()).span();
            resolved.push(ResolvedRef {
                def_start: def_span.start,
                def_end: def_span.end,
                use_start: use_span.start,
                use_end: use_span.end,
            });
        }
    }

    let mut imports: Vec<JsImport> = parsed
        .module_record
        .import_entries
        .iter()
        .filter_map(|entry| {
            let imported = match &entry.import_name {
                ImportImportName::Name(ns) => Some(ns.name.as_str().to_string()),
                ImportImportName::Default(_) => None,
                ImportImportName::NamespaceObject => return None,
            };
            Some(JsImport {
                local: entry.local_name.name.as_str().to_string(),
                specifier: entry.module_request.name.as_str().to_string(),
                imported,
                is_type: entry.is_type,
                local_start: entry.local_name.span.start,
                module_only: false,
            })
        })
        .collect();
    imports.extend(module_only_imports(&parsed.module_record, &imports));
    let required = require_imports(source, &semantic, &imports);
    imports.extend(required);

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
        reexports: reexports(&parsed.module_record),
        had_errors: parsed.fatal_error || !parsed.diagnostics.is_empty(),
    }
}

/// One `module_only` import per requested module that no binding import already names at the same
/// tier: side-effect imports, namespace-only imports and `export … from` re-exports, which the
/// import graph needs as file edges. A runtime request beside only type-only bindings still gets
/// one, so its edge is not marked type-only. Anchored at the first request of the specifier;
/// type-only when every request is.
fn module_only_imports(record: &ModuleRecord<'_>, bound: &[JsImport]) -> Vec<JsImport> {
    let any_bound: ahash::AHashSet<&str> = bound.iter().map(|import| import.specifier.as_str()).collect();
    let runtime_bound: ahash::AHashSet<&str> = bound
        .iter()
        .filter(|import| !import.is_type)
        .map(|import| import.specifier.as_str())
        .collect();
    let mut out: Vec<JsImport> = record
        .requested_modules
        .iter()
        .filter(|(specifier, requests)| {
            let covering = if requests.iter().all(|request| request.is_type) {
                &any_bound
            } else {
                &runtime_bound
            };
            !covering.contains(specifier.as_str())
        })
        .filter_map(|(specifier, requests)| {
            let first = requests.iter().min_by_key(|request| request.span.start)?;
            Some(JsImport {
                local: String::new(),
                specifier: specifier.as_str().to_string(),
                imported: None,
                is_type: requests.iter().all(|request| request.is_type),
                local_start: first.span.start,
                module_only: true,
            })
        })
        .collect();
    out.sort_unstable_by_key(|import| import.local_start);
    out
}

/// One `module_only` import per distinct literal `require('x')` the module makes whose specifier no
/// ESM import already names. Only the global `require` counts: a local binding of that name is some
/// other function. A call with a computed or template argument has no single target and is skipped.
fn require_imports(source: &str, semantic: &oxc_semantic::Semantic<'_>, known: &[JsImport]) -> Vec<JsImport> {
    let scoping = semantic.scoping();
    let mut seen: ahash::AHashSet<&str> = known.iter().map(|import| import.specifier.as_str()).collect();
    let mut out = Vec::new();
    for node in semantic.nodes().iter() {
        let AstKind::CallExpression(call) = node.kind() else {
            continue;
        };
        let (Expression::Identifier(callee), [Argument::StringLiteral(literal)]) = (&call.callee, &call.arguments[..])
        else {
            continue;
        };
        if callee.name.as_str() != "require" || scoping.get_reference(callee.reference_id()).symbol_id().is_some() {
            continue;
        }
        let Some(specifier) = source
            .get(literal.span.start as usize + 1..literal.span.end as usize - 1)
            .filter(|text| !text.is_empty() && !text.contains('\\'))
        else {
            continue;
        };
        if seen.insert(specifier) {
            out.push(JsImport {
                local: String::new(),
                specifier: specifier.to_string(),
                imported: None,
                is_type: false,
                local_start: literal.span.start,
                module_only: true,
            });
        }
    }
    out.sort_unstable_by_key(|import| import.local_start);
    out
}

/// The module's re-exports: every indirect export that forwards one named binding (`export { a as b }
/// from`, `export { a as default } from`, `export { x }` of an imported `x`), then every star export
/// (`export * from`). `export * as ns from` forwards no single binding, so it is not one.
fn reexports(record: &ModuleRecord<'_>) -> Vec<ReexportEdge> {
    let named = record.indirect_export_entries.iter().filter_map(|entry| {
        let (ExportImportName::Name(imported), ExportExportName::Name(exported)) =
            (&entry.import_name, &entry.export_name)
        else {
            return None;
        };
        let request = entry.module_request.as_ref()?;
        Some(ReexportEdge {
            specifier: request.name.as_str().to_string(),
            specifier_start: request.span.start,
            named: Some(NamedReexport {
                exported: exported.name.as_str().to_string(),
                imported: imported.name.as_str().to_string(),
            }),
            is_type: entry.is_type,
        })
    });
    let star = record.star_export_entries.iter().filter_map(|entry| {
        let request = entry.module_request.as_ref()?;
        Some(ReexportEdge {
            specifier: request.name.as_str().to_string(),
            specifier_start: request.span.start,
            named: None,
            is_type: entry.is_type,
        })
    });
    named.chain(star).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(source: &str) -> JsAnalysis {
        analyze(source, SourceType::ts())
    }

    #[test]
    fn resolves_call_to_its_definition() {
        let src = "function greet(name) {\n  return name;\n}\ngreet(\"hi\");\n";
        let a = ts(src);
        assert!(!a.had_errors);
        let greet_def = src.find("function greet").unwrap() + "function ".len();
        let greet_call = src.rfind("greet(").unwrap();
        let call_ref = a
            .resolved
            .iter()
            .find(|r| r.use_start as usize == greet_call)
            .expect("greet call must resolve");
        assert_eq!(
            call_ref.def_start as usize, greet_def,
            "greet call binds to the function def"
        );
    }

    #[test]
    fn shadowing_resolves_to_inner_binding() {
        let src = "const x = 1;\nfunction f() {\n  const x = 2;\n  return x;\n}\n";
        let a = ts(src);
        let outer_x = src.find("const x = 1").unwrap() + "const ".len();
        let inner_x = src.find("const x = 2").unwrap() + "const ".len();
        let use_x = src.rfind("return x").unwrap() + "return ".len();
        let r = a
            .resolved
            .iter()
            .find(|r| r.use_start as usize == use_x)
            .expect("inner x use must resolve");
        assert_eq!(r.def_start as usize, inner_x, "must bind to inner x, not outer");
        assert_ne!(r.def_start as usize, outer_x);
    }

    #[test]
    fn literal_require_calls_are_module_only_imports() {
        let src = "const a = require('./a');\nconst b = require(`./b`);\nconst c = require(name);\nrequire('./a');\nimport d from './d';\nrequire('./d');\n";
        let analysis = analyze(src, SourceType::ts());
        let required: Vec<(&str, bool)> = analysis
            .imports
            .iter()
            .map(|import| (import.specifier.as_str(), import.module_only))
            .collect();
        assert_eq!(required, vec![("./d", false), ("./a", true)]);
    }

    #[test]
    fn a_locally_bound_require_is_not_an_import() {
        let src = "function f(require) { return require('./a'); }\nconst g = (require) => require('./b');\n";
        assert!(analyze(src, SourceType::ts()).imports.is_empty());
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

        assert!(
            by_local("ns").is_none(),
            "namespace import binds no export site to stitch"
        );
        let pkg = a
            .imports
            .iter()
            .find(|i| i.specifier == "pkg")
            .expect("namespace import is a module-only import");
        assert!(pkg.module_only && pkg.local.is_empty() && pkg.imported.is_none());
        assert_eq!(
            a.imports.iter().filter(|i| i.module_only).count(),
            1,
            "a specifier with a binding import gets no module-only import"
        );
    }

    #[test]
    fn side_effect_imports_and_reexports_are_module_only_imports() {
        let src = "import './polyfill';
export * from './all';
export { a as b } from './named';
export type { T } from './types';
import { x } from './x';
export { x } from './x';
import type { U } from './u';
export * from './u';
";
        let a = ts(src);
        let module_only: Vec<(&str, bool, u32)> = a
            .imports
            .iter()
            .filter(|i| i.module_only)
            .map(|i| (i.specifier.as_str(), i.is_type, i.local_start))
            .collect();
        let at = |needle: &str| src.find(needle).unwrap() as u32;
        assert_eq!(
            module_only,
            [
                ("./polyfill", false, at("'./polyfill'")),
                ("./all", false, at("'./all'")),
                ("./named", false, at("'./named'")),
                ("./types", true, at("'./types'")),
                ("./u", false, at("'./u'")),
            ]
        );
        assert!(
            a.imports.iter().any(|i| i.specifier == "./x" && !i.module_only),
            "a specifier with a binding keeps only its binding import"
        );
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
    fn extracts_named_and_star_reexports() {
        let src = "import { x } from './x';
export { x };
export { a as b, c } from './named';
export { d as default } from './dflt';
export type { T } from './types';
export * from './all';
export * as ns from './ns';
export type * from './typestar';
";
        let a = ts(src);
        assert!(!a.had_errors);
        let at = |needle: &str| src.find(needle).unwrap() as u32;
        let named = |specifier: &str, exported: &str, imported: &str, is_type: bool| ReexportEdge {
            specifier: specifier.to_string(),
            specifier_start: at(&format!("'{specifier}'")),
            named: Some(NamedReexport {
                exported: exported.to_string(),
                imported: imported.to_string(),
            }),
            is_type,
        };
        let star = |specifier: &str, is_type: bool| ReexportEdge {
            specifier: specifier.to_string(),
            specifier_start: at(&format!("'{specifier}'")),
            named: None,
            is_type,
        };
        assert_eq!(
            a.reexports,
            [
                named("./x", "x", "x", false),
                named("./named", "b", "a", false),
                named("./named", "c", "c", false),
                named("./dflt", "default", "d", false),
                named("./types", "T", "T", true),
                star("./all", false),
                star("./typestar", true),
            ],
            "`export * as ns from` forwards no single binding, so it is no re-export"
        );
        assert!(
            a.exports.is_empty(),
            "a re-export is not a direct export: {:?}",
            a.exports
        );
    }

    #[test]
    fn free_identifier_is_not_resolved() {
        let src = "function f() {\n  return fetch('/x');\n}\n";
        let a = ts(src);
        let fetch_use = src.find("fetch(").unwrap() as u32;
        assert!(
            !a.resolved.iter().any(|r| r.use_start == fetch_use),
            "global `fetch` must not resolve to an in-file def"
        );
    }
}
