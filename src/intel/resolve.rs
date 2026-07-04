//! Per-file resolution dispatch: turn a file's source into [`FileResolvedRefs`] using the best
//! available engine for its language.
//!
//! - **JS/TS/JSX/TSX** (feature `code-intel-js`) → oxc: full scope + import/export resolution
//!   with precise use/def spans. Needs no tree-sitter grammar.
//! - **everything else** → tree-sitter `locals`: intra-file scope binding across ~80+ grammars.
//!
//! This is the compute half of the scanner's second pass; the pass itself (caching + index
//! staging) lives in `src/scanner.rs`.

use std::path::Path;

use crate::intel::model::{FileResolvedRefs, ResolvedEdge};
use crate::lang::LangId;

/// TSLP pack names oxc handles. JSX lives under the `javascript` grammar; `.jsx`/`.tsx`/`.mjs`
/// source-type nuances are resolved from the path by `source_type_for_path`.
#[cfg(feature = "code-intel-js")]
fn is_js_ts(lang: LangId) -> bool {
    matches!(lang, "javascript" | "typescript" | "tsx")
}

/// Compute a file's resolution facts. Never fails — a parse error or unsupported language yields
/// empty facts (the caller simply writes no resolved edges for that file).
pub fn resolve_file(lang: LangId, path: &Path, source: &[u8]) -> FileResolvedRefs {
    #[cfg(feature = "code-intel-js")]
    if is_js_ts(lang)
        && let Some(refs) = resolve_js(lang, path, source)
    {
        return refs;
    }
    #[cfg(not(feature = "code-intel-js"))]
    let _ = path; // path only drives JS/TS source-type selection, which is feature-gated
    resolve_via_locals(lang, source)
}

/// oxc-backed resolution for JS/TS. Returns `None` when the source isn't UTF-8 or the path isn't
/// a recognized JS/TS extension (the dispatcher then falls back to locals).
#[cfg(feature = "code-intel-js")]
fn resolve_js(lang: LangId, path: &Path, source: &[u8]) -> Option<FileResolvedRefs> {
    use crate::intel::model::{ExportEdge, ImportEdge};

    let src = std::str::from_utf8(source).ok()?;
    let source_type = crate::intel::js::source_type_for_path(path)?;
    let analysis = crate::intel::js::analyze(src, source_type);

    let mut out = FileResolvedRefs::new(lang);
    out.intra = analysis
        .resolved
        .iter()
        .map(|r| ResolvedEdge {
            use_start: r.use_start,
            use_end: r.use_end,
            def_start: r.def_start,
            def_end: r.def_end,
        })
        .collect();
    out.imports = analysis
        .imports
        .into_iter()
        .map(|i| ImportEdge {
            local: i.local,
            specifier: i.specifier,
            imported: i.imported,
            is_type: i.is_type,
            local_start: i.local_start,
        })
        .collect();
    out.exports = analysis
        .exports
        .into_iter()
        .map(|e| ExportEdge {
            name: e.name,
            name_start: e.name_start,
        })
        .collect();
    Some(out)
}

/// tree-sitter `locals`-backed intra-file resolution for any language with a locals query.
///
/// The index only needs start offsets, which `LocalBindings` provides. Span ends for the
/// locals path are set equal to their starts for now — enriched when `LocalBindings` grows to
/// carry identifier ends in the follow-up that fully wires + grammar-tests the locals path.
fn resolve_via_locals(lang: LangId, source: &[u8]) -> FileResolvedRefs {
    use crate::lang::{ParseOutcome, parse_with_default_timeout, with_parser};

    let mut out = FileResolvedRefs::new(lang);
    let tree = match with_parser(lang, |p| parse_with_default_timeout(p, source)) {
        Ok(ParseOutcome::Ok(tree)) => tree,
        _ => return out, // parse failure / timeout / unknown lang → empty facts
    };
    let Ok(bindings) = crate::extract::locals::resolve_locals(lang, &tree, source) else {
        return out;
    };
    out.intra = bindings
        .edges()
        .map(|(use_start, def_start)| ResolvedEdge {
            use_start,
            use_end: use_start,
            def_start,
            def_end: def_start,
        })
        .collect();
    out
}

#[cfg(all(test, feature = "code-intel-js"))]
mod tests {
    use super::*;

    #[test]
    fn resolve_file_js_yields_intra_edges_and_imports() {
        // `x` used in the return resolves to the local `const x`; the import edge is captured.
        let src = b"import { helper } from './util';\nfunction f() {\n  const x = 1;\n  return x + helper();\n}\n";
        let refs = resolve_file("typescript", Path::new("app.ts"), src);

        let x_def = b"import { helper } from './util';\nfunction f() {\n  const ".len() as u32;
        assert!(
            refs.intra.iter().any(|e| e.def_start == x_def),
            "the `x` use must resolve to the local `const x` definition"
        );
        assert!(
            refs.imports
                .iter()
                .any(|i| i.local == "helper" && i.specifier == "./util"),
            "the `helper` import edge must be captured"
        );
    }

    #[test]
    fn resolve_file_non_utf8_yields_empty() {
        // Invalid UTF-8 in a .ts file: the oxc path bails to None, locals can't parse → empty.
        let refs = resolve_file("typescript", Path::new("bad.ts"), &[0xff, 0xfe, 0x00]);
        assert!(refs.is_empty(), "non-UTF-8 source must yield empty facts, not panic");
    }
}
