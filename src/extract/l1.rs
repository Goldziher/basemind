use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, QueryMatch};

use super::{ExtractError, FileMapL1, Implementation, Import, SCHEMA_VER, Symbol, SymbolKind};
use crate::lang::{
    LangId, ParseOutcome, QueryKind, parse_with_default_timeout, try_get_query, with_parser,
};

pub fn extract_l1(lang: LangId, source: &[u8]) -> Result<FileMapL1, ExtractError> {
    // tree-sitter recovers from syntax errors and returns a partial Tree; we expect
    // `has_error()` may be true and still extract what we can. The timeout guards against
    // inputs that hang the parser's error-recovery loop.
    let outcome = with_parser(lang, |p| parse_with_default_timeout(p, source))?;
    let tree = match outcome {
        ParseOutcome::Ok(t) => t,
        ParseOutcome::Failed => return Err(ExtractError::ParseFailure),
        ParseOutcome::TimedOut => {
            return Err(ExtractError::ParseTimeout(
                crate::lang::DEFAULT_PARSE_TIMEOUT,
            ));
        }
    };
    let root = tree.root_node();

    let (had_errors, error_count) = if root.has_error() {
        (true, count_error_nodes(root))
    } else {
        (false, 0)
    };

    let symbols = run_symbols(lang, root, source)?;
    let imports = run_imports(lang, root, source)?;
    let implementations = run_implementations(lang, root, source)?;

    Ok(FileMapL1 {
        schema_ver: SCHEMA_VER,
        language: lang.to_string(),
        size_bytes: source.len() as u64,
        had_errors,
        error_count,
        symbols,
        imports,
        implementations,
    })
}

/// Count nodes in the tree that are tree-sitter ERROR or MISSING markers.
/// Single iterative DFS — avoids recursion blowing the stack on deeply nested code.
fn count_error_nodes(root: Node) -> u32 {
    let mut count: u32 = 0;
    let mut cursor = root.walk();
    let mut stack: Vec<Node> = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            count = count.saturating_add(1);
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    count
}

fn run_symbols(
    lang: LangId,
    root: tree_sitter::Node,
    source: &[u8],
) -> Result<Vec<Symbol>, ExtractError> {
    let Some(q) = try_get_query(lang, QueryKind::Symbols)? else {
        return Ok(Vec::new());
    };
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&q, root, source);
    let mut out = Vec::new();
    while let Some(m) = iter.next() {
        if let Some(sym) = build_symbol(&q, m, source) {
            out.push(sym);
        }
    }
    Ok(dedupe_symbols(out))
}

/// Merge query matches that hit the same (`start_byte`, `name`) — happens when a generic
/// pattern (e.g. `const X = …` → `Const`) and a specific pattern (e.g. `const X = () => …`
/// → `Function`) both fire on one declaration. Higher `specificity()` wins; document
/// order is preserved.
///
/// O(n) via an `AHashMap` keyed by `(start_byte, name)`. The earlier O(n²) `iter_mut().find`
/// implementation cost ~100 µs on files with >500 symbols; this hash-lookup form stays under
/// 5 µs on the same input. The map key clones `name` once per *new* symbol — the existing
/// symbols already owned their name, so the dedupe step does not introduce additional
/// `String` allocations beyond what tree-sitter extraction produced.
fn dedupe_symbols(syms: Vec<Symbol>) -> Vec<Symbol> {
    let mut keep: Vec<Symbol> = Vec::with_capacity(syms.len());
    let mut index: ahash::AHashMap<(u32, String), usize> =
        ahash::AHashMap::with_capacity(syms.len());
    for sym in syms {
        let key = (sym.start_byte, sym.name.clone());
        if let Some(&idx) = index.get(&key) {
            let existing = &mut keep[idx];
            if sym.kind.specificity() > existing.kind.specificity() {
                existing.kind = sym.kind;
                // Prefer the more specific match's signature too — e.g. an arrow-fn pattern
                // captures the whole `const F = (x) => …` line, which is the useful signature.
                if sym.signature.is_some() {
                    existing.signature = sym.signature;
                }
            }
            // Decorator captures travel one-per-match (tree-sitter fires the
            // `(decorator) @symbol.decorator` pattern once per decorator child of a
            // decorated_definition), so on collision we union the lists — deduplicated by
            // string, preserving first-seen order.
            for d in sym.decorators {
                if !existing.decorators.contains(&d) {
                    existing.decorators.push(d);
                }
            }
        } else {
            let new_idx = keep.len();
            keep.push(sym);
            index.insert(key, new_idx);
        }
    }
    keep
}

fn run_imports(
    lang: LangId,
    root: tree_sitter::Node,
    source: &[u8],
) -> Result<Vec<Import>, ExtractError> {
    let Some(q) = try_get_query(lang, QueryKind::Imports)? else {
        return Ok(Vec::new());
    };
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&q, root, source);
    let mut out = Vec::new();
    while let Some(m) = iter.next() {
        if let Some(imp) = build_import(&q, m, source) {
            out.push(imp);
        }
    }
    Ok(out)
}

fn capture_name(q: &Query, index: u32) -> &str {
    q.capture_names()[index as usize]
}

fn build_symbol(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<Symbol> {
    let mut name: Option<String> = None;
    let mut kind: Option<SymbolKind> = None;
    let mut start_byte = 0u32;
    let mut end_byte = 0u32;
    let mut start_row = 0u32;
    let mut start_col = 0u32;
    let mut signature: Option<String> = None;
    let mut decorators: Vec<String> = Vec::new();

    for cap in m.captures {
        let cname = capture_name(q, cap.index);
        let node = cap.node;
        if cname == "symbol.name" {
            name = node.utf8_text(source).ok().map(|s| s.to_string());
        } else if cname == "symbol.decorator" {
            // Decorator captures travel alongside their owning symbol — collect them all.
            if let Ok(text) = node.utf8_text(source) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    decorators.push(trimmed.to_string());
                }
            }
        } else if let Some(suffix) = cname.strip_prefix("symbol.") {
            kind = Some(SymbolKind::from_capture_suffix(suffix));
            start_byte = node.start_byte() as u32;
            end_byte = node.end_byte() as u32;
            let p = node.start_position();
            start_row = p.row as u32;
            start_col = p.column as u32;
            if let Ok(text) = node.utf8_text(source) {
                signature = signature_slice(text);
                if matches!(kind, Some(SymbolKind::Method))
                    && let Some(promoted) = detect_accessor(text)
                {
                    kind = Some(promoted);
                }
            }
        }
    }

    Some(Symbol {
        name: name?,
        kind: kind.unwrap_or(SymbolKind::Unknown),
        start_byte,
        end_byte,
        start_row,
        start_col,
        signature,
        decorators,
    })
}

/// Promote a `method_definition` capture to `Getter` or `Setter` when the source slice
/// starts with the `get`/`set` keyword (after skipping any leading modifier keywords).
/// Matching the accessor `kind` field directly in tree-sitter queries is fragile across
/// grammar versions, so we look at the bytes instead. Token scan caps at 8 to bound work
/// on pathological input.
fn detect_accessor(slice: &str) -> Option<SymbolKind> {
    for tok in slice.split_whitespace().take(8) {
        match tok {
            "get" => return Some(SymbolKind::Getter),
            "set" => return Some(SymbolKind::Setter),
            "static" | "public" | "private" | "protected" | "readonly" | "override" | "async" => {
                continue;
            }
            _ => return None,
        }
    }
    None
}

/// Reduce a symbol's full body text down to a single-line signature header.
///
/// Strategy: walk byte-by-byte from the start of the node's text until we hit the first
/// `{` (function/class/interface body) or `;` (statement terminator for type aliases,
/// const declarations, interface members). Everything before that becomes the signature,
/// with internal whitespace runs collapsed to single spaces — this keeps multi-line
/// generic parameter lists readable as `function foo< T extends Bar, U > (x): T`.
///
/// Returns `None` for empty/whitespace-only signatures so callers can leave the field unset.
fn signature_slice(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' || b == b';' {
            end = i;
            break;
        }
    }
    let collapsed: String = text[..end].split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

fn run_implementations(
    lang: LangId,
    root: tree_sitter::Node,
    source: &[u8],
) -> Result<Vec<Implementation>, ExtractError> {
    let Some(q) = try_get_query(lang, QueryKind::Implementations)? else {
        return Ok(Vec::new());
    };
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&q, root, source);
    let mut out = Vec::new();
    while let Some(m) = iter.next() {
        if let Some(imp) = build_implementation(&q, m, source) {
            out.push(imp);
        }
    }
    Ok(out)
}

/// Build an `Implementation` from a query match that contains:
/// - `@impl.trait_name` — the parent / trait / interface identifier node.
/// - `@impl.implementor` (optional) — the implementing type identifier node.
///   When absent (TSLP-adapted patterns), the implementor is inferred by walking
///   the `@impl.range` node's named ancestors to find the nearest identifier.
/// - `@impl.range` (optional) — the containing declaration node; used for position
///   when `@impl.trait_name` itself carries no useful start position.
fn build_implementation(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<Implementation> {
    let mut trait_name: Option<String> = None;
    let mut impl_type: Option<String> = None;
    let mut range_node: Option<Node> = None;
    let mut trait_node: Option<Node> = None;

    for cap in m.captures {
        let cname = capture_name(q, cap.index);
        match cname {
            "impl.trait_name" => {
                trait_name = cap.node.utf8_text(source).ok().map(|s| s.to_string());
                trait_node = Some(cap.node);
            }
            "impl.implementor" => {
                impl_type = cap.node.utf8_text(source).ok().map(|s| s.to_string());
            }
            "impl.range" => {
                range_node = Some(cap.node);
            }
            _ => {}
        }
    }

    let trait_name = trait_name?;

    // If the query didn't provide an explicit @impl.implementor, walk up the
    // @impl.range (or @impl.trait_name node's parent) to find the nearest named
    // identifier that looks like a type name. This is the TSLP-adapted fallback.
    let impl_type = impl_type.or_else(|| {
        let anchor = range_node.or(trait_node)?;
        implementor_from_ancestor(anchor, source)
    })?;

    // Position: prefer the range node, fall back to the trait_name node.
    let pos_node = range_node.or(trait_node)?;
    let p = pos_node.start_position();

    Some(Implementation {
        trait_name,
        impl_type,
        start_byte: pos_node.start_byte() as u32,
        start_row: p.row as u32,
        start_col: p.column as u32,
    })
}

/// Walk up the tree from `node` to find the nearest named ancestor that has an
/// identifier / type-identifier child in the `name` field. Returns the text of
/// that identifier. Used to infer the implementing type from TSLP patterns that
/// only capture the trait name and the whole expression node.
fn implementor_from_ancestor(node: Node, source: &[u8]) -> Option<String> {
    /// Extract non-empty text from a node field, returning `None` if absent or empty.
    fn field_text<'a>(parent: Node<'a>, field: &str, src: &'a [u8]) -> Option<&'a str> {
        let n = parent.child_by_field_name(field)?;
        let t = n.utf8_text(src).ok()?;
        if t.is_empty() { None } else { Some(t) }
    }

    // Walk up at most 8 levels to bound work on pathological trees.
    let mut current = node;
    for _ in 0..8 {
        let parent = current.parent()?;
        // Try `name:` field first — present on class_declaration, struct_item, etc.
        if let Some(text) = field_text(parent, "name", source) {
            return Some(text.to_string());
        }
        // Try `type:` field — present on impl_item (Rust). Only use leaf-like nodes
        // (type_identifier, identifier) to avoid capturing the full type expression.
        if let Some(type_node) = parent.child_by_field_name("type") {
            // `child_count() == 0` guards against complex type expressions.
            let leaf_text = (type_node.child_count() == 0)
                .then(|| type_node.utf8_text(source).ok())
                .flatten()
                .filter(|t| !t.is_empty());
            if let Some(text) = leaf_text {
                return Some(text.to_string());
            }
        }
        current = parent;
    }
    None
}

fn build_import(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<Import> {
    let mut range_node = None;
    let mut module: Option<String> = None;

    for cap in m.captures {
        let cname = capture_name(q, cap.index);
        match cname {
            "import.range" => range_node = Some(cap.node),
            "import.module" => {
                module = cap.node.utf8_text(source).ok().map(|s| s.to_string());
            }
            _ => {}
        }
    }

    let node = range_node?;
    let raw = node.utf8_text(source).ok()?.to_string();
    Some(Import {
        module,
        raw,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_implementation_rust_trait_impl() {
        // `impl Drawable for Beta` must produce one Implementation record.
        let src = br#"
trait Drawable {
    fn draw(&self);
}

struct Beta {
    x: i32,
}

impl Drawable for Beta {
    fn draw(&self) {}
}
"#;
        let map = extract_l1("rust", src).expect("extract");
        let impls = &map.implementations;
        assert!(
            !impls.is_empty(),
            "expected at least one Implementation; got none"
        );
        let found = impls
            .iter()
            .find(|i| i.trait_name == "Drawable" && i.impl_type == "Beta");
        assert!(
            found.is_some(),
            "expected Implementation {{ trait_name: \"Drawable\", impl_type: \"Beta\" }}; got {impls:?}"
        );
    }

    #[test]
    fn extract_basic_rust() {
        let src = br#"
pub fn hello() {}

pub struct Foo {
    x: i32,
}

use std::collections::HashMap;

const N: u32 = 42;
"#;
        let map = extract_l1("rust", src).expect("extract");
        let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"N"));
        assert!(!map.imports.is_empty(), "expected at least one import");
        assert!(!map.had_errors, "clean source must not flag errors");
        assert_eq!(map.error_count, 0);
    }

    #[test]
    fn extract_recovers_from_syntax_errors() {
        // Broken `fn broken( {` plus well-formed neighbors.
        let src = br#"
pub fn good_one() {}

pub fn broken( {
    let x = ;
}

pub fn good_two() {}
"#;
        let map = extract_l1("rust", src).expect("extract should not fail on partial parse");
        assert!(
            map.had_errors,
            "had_errors should be true for syntax errors"
        );
        assert!(
            map.error_count > 0,
            "error_count should be > 0; got {}",
            map.error_count
        );
        let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"good_one") || names.contains(&"good_two"),
            "at least one well-formed sibling symbol should be recovered; got {names:?}"
        );
    }
}
