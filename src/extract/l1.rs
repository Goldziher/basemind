use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, QueryMatch};

use super::{ExtractError, FileMapL1, Import, SCHEMA_VER, Symbol, SymbolKind};
use crate::lang::{
    Lang, ParseOutcome, QueryKind, get_query, parse_with_default_timeout, with_parser,
};

pub fn extract_l1(lang: Lang, source: &[u8]) -> Result<FileMapL1, ExtractError> {
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

    Ok(FileMapL1 {
        schema_ver: SCHEMA_VER,
        language: lang.name().to_string(),
        size_bytes: source.len() as u64,
        had_errors,
        error_count,
        symbols,
        imports,
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
    lang: Lang,
    root: tree_sitter::Node,
    source: &[u8],
) -> Result<Vec<Symbol>, ExtractError> {
    let q = get_query(lang, QueryKind::Symbols)?;
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
fn dedupe_symbols(syms: Vec<Symbol>) -> Vec<Symbol> {
    let mut keep: Vec<Symbol> = Vec::with_capacity(syms.len());
    for sym in syms {
        if let Some(existing) = keep
            .iter_mut()
            .find(|s| s.start_byte == sym.start_byte && s.name == sym.name)
        {
            if sym.kind.specificity() > existing.kind.specificity() {
                existing.kind = sym.kind;
                // Prefer the more specific match's signature too — e.g. an arrow-fn pattern
                // captures the whole `const F = (x) => …` line, which is the useful signature.
                if sym.signature.is_some() {
                    existing.signature = sym.signature;
                }
            }
        } else {
            keep.push(sym);
        }
    }
    keep
}

fn run_imports(
    lang: Lang,
    root: tree_sitter::Node,
    source: &[u8],
) -> Result<Vec<Import>, ExtractError> {
    let q = get_query(lang, QueryKind::Imports)?;
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

    for cap in m.captures {
        let cname = capture_name(q, cap.index);
        let node = cap.node;
        if cname == "symbol.name" {
            name = node.utf8_text(source).ok().map(|s| s.to_string());
        } else if let Some(suffix) = cname.strip_prefix("symbol.") {
            kind = Some(SymbolKind::from_capture_suffix(suffix));
            start_byte = node.start_byte() as u32;
            end_byte = node.end_byte() as u32;
            let p = node.start_position();
            start_row = p.row as u32;
            start_col = p.column as u32;
            if let Ok(text) = node.utf8_text(source) {
                // Find the first newline directly on the byte slice — avoids the per-char
                // search `str::lines` performs over potentially-large symbol bodies.
                let bytes = text.as_bytes();
                let end = memchr::memchr(b'\n', bytes).unwrap_or(bytes.len());
                let first = text[..end].trim_end_matches('\r').trim();
                if !first.is_empty() {
                    signature = Some(first.to_string());
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
    })
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
    fn extract_basic_rust() {
        let src = br#"
pub fn hello() {}

pub struct Foo {
    x: i32,
}

use std::collections::HashMap;

const N: u32 = 42;
"#;
        let map = extract_l1(Lang::Rust, src).expect("extract");
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
        let map = extract_l1(Lang::Rust, src).expect("extract should not fail on partial parse");
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
