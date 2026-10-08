use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryMatch};

use super::{ExtractError, FileMapL1, Implementation, Import, SCHEMA_VER, Symbol, SymbolKind, capture_name};
use crate::lang::{
    CaptureClass, LangId, ParseOutcome, parse_with_default_timeout, try_get_classified_combined_l1_query, with_parser,
};

pub fn extract_l1(lang: LangId, source: &[u8]) -> Result<FileMapL1, ExtractError> {
    let outcome = with_parser(lang, |p| parse_with_default_timeout(p, source))?;
    let tree = match outcome {
        ParseOutcome::Ok(t) => t,
        ParseOutcome::Failed => return Err(ExtractError::ParseFailure),
        ParseOutcome::TimedOut => {
            return Err(ExtractError::ParseTimeout(crate::lang::DEFAULT_PARSE_TIMEOUT));
        }
    };
    extract_l1_from_tree(lang, &tree, source)
}

/// Extract L1 data from a pre-parsed tree-sitter `Tree`. Separated from `extract_l1` so the
/// scanner can share one parse between L1 and L2 when eager L2 is enabled, avoiding a second
/// full parse per file on the hot path.
pub(crate) fn extract_l1_from_tree(
    lang: LangId,
    tree: &tree_sitter::Tree,
    source: &[u8],
) -> Result<FileMapL1, ExtractError> {
    let root = tree.root_node();

    let (had_errors, error_count) = if root.has_error() {
        (true, count_error_nodes(root))
    } else {
        (false, 0)
    };

    let (symbols, imports, implementations) = run_combined(lang, root, source)?;
    let rationale = super::rationale::extract_rationale(root, source);

    Ok(FileMapL1 {
        schema_ver: SCHEMA_VER,
        language: lang.to_string(),
        size_bytes: source.len() as u64,
        had_errors,
        error_count,
        symbols,
        imports,
        implementations,
        rationale,
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

/// Output triple of `run_combined`: symbols, imports, and implementations extracted in one pass.
type CombinedL1 = (Vec<Symbol>, Vec<Import>, Vec<Implementation>);

/// Walk the combined L1 query (symbols + imports + implementations) once, dispatching each
/// match by pre-classified capture index. Allocates one `QueryCursor` instead of three, cutting
/// the per-file tree-walk cost to one pass. Dispatch is an integer array lookup
/// (`classes[first_cap.index]`) instead of three `starts_with` comparisons per match.
fn run_combined(lang: LangId, root: tree_sitter::Node, source: &[u8]) -> Result<CombinedL1, ExtractError> {
    let Some(cq) = try_get_classified_combined_l1_query(lang)? else {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    };
    let q = &cq.query;
    let classes = &cq.classes;
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut implementations = Vec::new();
    crate::lang::with_query_cursor(|cursor| {
        let mut iter = cursor.matches(q, root, source);
        while let Some(m) = iter.next() {
            let class = m
                .captures()
                .iter()
                .map(|cap| classes[cap.index as usize])
                .find(|class| !matches!(class, CaptureClass::Other));
            match class {
                Some(CaptureClass::Symbol) => {
                    if let Some(sym) = build_symbol(q, m, source) {
                        symbols.push(sym);
                    }
                }
                Some(CaptureClass::Import) => {
                    if let Some(imp) = build_import(q, m, source) {
                        imports.push(imp);
                    }
                }
                Some(CaptureClass::Impl) => {
                    if let Some(imp) = build_implementation(q, m, source) {
                        implementations.push(imp);
                    }
                }
                Some(CaptureClass::Other) | None => {}
            }
        }
    });
    let mut symbols = dedupe_symbols(symbols);
    for sym in &mut symbols {
        trim_signature(sym);
    }
    Ok((symbols, imports, implementations))
}

/// Merge query matches that hit the same (`start_byte`, `name`) — happens when a generic
/// pattern (e.g. `const X = …` → `Const`) and a specific pattern (e.g. `const X = () => …`
/// → `Function`) both fire on one declaration. Higher `specificity()` wins; document
/// order is preserved.
///
/// O(n) via an `AHashMap` keyed by `start_byte`, with a small inner `Vec<(name, kept_index)>`
/// to disambiguate the (near-impossible) case of two distinct names sharing one `start_byte`.
/// The earlier O(n²) `iter_mut().find` implementation cost ~100 µs on files with >500 symbols;
/// this hash-lookup form stays under 5 µs on the same input. Probing by `start_byte` lets us
/// match against a borrowed `&str` name, so the name is cloned only when inserting a brand-new
/// entry — not on every probe. The inner Vec is almost always length 1 (name collisions at the
/// same start byte are vanishingly rare), so the linear scan is effectively O(1).
fn dedupe_symbols(syms: Vec<Symbol>) -> Vec<Symbol> {
    let mut keep: Vec<Symbol> = Vec::with_capacity(syms.len());
    let mut index: ahash::AHashMap<u32, Vec<(String, usize)>> = ahash::AHashMap::with_capacity(syms.len());
    for sym in syms {
        let slot = index.entry(sym.start_byte).or_default();
        if let Some(&(_, idx)) = slot.iter().find(|(name, _)| name == &sym.name) {
            let existing = &mut keep[idx];
            if sym.kind.specificity() > existing.kind.specificity() {
                existing.kind = sym.kind;
                if sym.signature.is_some() {
                    existing.signature = sym.signature;
                }
            }
            for d in sym.decorators {
                if !existing.decorators.contains(&d) {
                    existing.decorators.push(d);
                }
            }
        } else {
            slot.push((sym.name.clone(), keep.len()));
            keep.push(sym);
        }
    }
    keep
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
    let mut symbol_node: Option<Node> = None;

    for cap in m.captures() {
        let cname = capture_name(q, cap.index);
        let node = cap.node;
        if cname == "symbol.name" {
            name = node.utf8_text(source).ok().map(|s| s.to_string());
        } else if cname == "symbol.decorator" {
            if let Ok(text) = node.utf8_text(source) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    decorators.push(trimmed.to_string());
                }
            }
        } else if let Some(suffix) = cname.strip_prefix("symbol.") {
            kind = Some(SymbolKind::from_capture_suffix(suffix));
            symbol_node = Some(node);
            start_byte = node.start_byte() as u32;
            end_byte = node.end_byte() as u32;
            let p = node.start_position();
            start_row = p.row as u32;
            start_col = p.column as u32;
            if let Ok(text) = node.utf8_text(source) {
                signature = signature_slice(header_text(node, text));
                if matches!(kind, Some(SymbolKind::Method))
                    && let Some(promoted) = detect_accessor(text)
                {
                    kind = Some(promoted);
                }
            }
        }
    }

    // The outline is a map of what a file exposes, not of what its function bodies compute:
    // bindings and nested definitions inside a function/method/lambda are implementation detail.
    if symbol_node.is_some_and(is_function_local) {
        return None;
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

/// Node kinds (across the bundled grammars) whose subtree is a function, method or lambda body.
/// Matched by kind name so the check is language-generic; a kind that a grammar does not use
/// simply never matches.
const FUNCTION_SCOPE_KINDS: &[&str] = &[
    "function_definition",
    "function_declaration",
    "function_item",
    "function_expression",
    "function",
    "generator_function",
    "generator_function_declaration",
    "arrow_function",
    "method_definition",
    "method_declaration",
    "method",
    "singleton_method",
    "constructor_declaration",
    "lambda",
    "lambda_expression",
    "lambda_literal",
    "closure_expression",
    "func_literal",
    "function_literal",
    "local_function_statement",
    "anonymous_function",
    "anonymous_function_creation_expression",
];

/// True when `node` sits inside a function-like scope (it has a function/method/lambda ancestor).
/// The node itself being a function does not count: top-level and class-level functions and methods
/// are kept, as are class fields and module-level bindings; function locals and nested definitions
/// are not.
fn is_function_local(node: Node) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if FUNCTION_SCOPE_KINDS.contains(&n.kind()) {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// Longest signature kept for a value-like symbol (variable, constant, field). Their "signature" is
/// the whole initialiser, which can be a multi-line call; the outline only needs its shape.
const VALUE_SIGNATURE_MAX_CHARS: usize = 80;

/// Drop signature text that costs tokens without adding information: for value-like symbols the
/// name is already a field of the entry, so `MAX_RETRIES = 3` is stored as `= 3`, and a long
/// initialiser is cut at [`VALUE_SIGNATURE_MAX_CHARS`]. Definitions (functions, classes ...) keep
/// their full header.
fn trim_signature(sym: &mut Symbol) {
    if !matches!(sym.kind, SymbolKind::Variable | SymbolKind::Const | SymbolKind::Field) {
        return;
    }
    let Some(sig) = sym.signature.as_deref() else {
        return;
    };
    let mut out = match sig.find(sym.name.as_str()) {
        Some(i) if whole_word(sig, i, sym.name.len()) => {
            let mut t = String::with_capacity(sig.len());
            t.push_str(sig[..i].trim_end());
            if !t.is_empty() {
                t.push(' ');
            }
            t.push_str(sig[i + sym.name.len()..].trim_start());
            t
        }
        _ => sig.to_string(),
    };
    if out.chars().count() > VALUE_SIGNATURE_MAX_CHARS {
        let cut = out
            .char_indices()
            .nth(VALUE_SIGNATURE_MAX_CHARS)
            .map_or(out.len(), |(i, _)| i);
        out.truncate(cut);
        out.push('…');
    }
    sym.signature = if out.is_empty() { None } else { Some(out) };
}

fn whole_word(s: &str, start: usize, len: usize) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let before = s[..start].chars().next_back().is_none_or(|c| !is_word(c));
    let after = s[start + len..].chars().next().is_none_or(|c| !is_word(c));
    before && after
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

/// The part of a definition's text that precedes its body, when the grammar names the body.
///
/// The brace/semicolon scan in [`signature_slice`] suits C-family syntax but not indentation-based
/// languages: a Python `def` has no `{`, so its whole body (up to the first dict literal or `;`)
/// became the "signature", several times larger than the declaration it summarises. A `body` field
/// ends the header exactly where the grammar says the body begins.
fn header_text<'a>(node: Node, text: &'a str) -> &'a str {
    node.child_by_field_name("body")
        .map(|body| body.start_byte().saturating_sub(node.start_byte()))
        .and_then(|len| text.get(..len))
        .unwrap_or(text)
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
    if let Some(i) = memchr::memchr2(b'{', b';', bytes) {
        end = i;
    }
    let slice = &text[..end];

    if is_already_collapsed(slice) {
        return if slice.is_empty() {
            None
        } else {
            Some(slice.to_string())
        };
    }

    let mut collapsed = String::with_capacity(slice.len());
    for word in slice.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }
    if collapsed.is_empty() { None } else { Some(collapsed) }
}

/// True when `split_whitespace` would reproduce `slice` byte-for-byte: the slice is pure ASCII,
/// has no leading/trailing whitespace, and contains no run of two-or-more whitespace bytes nor
/// any whitespace byte other than a single space (tab, newline, etc.). Conservatively requires
/// ASCII so byte-level whitespace checks fully cover Unicode-whitespace semantics.
fn is_already_collapsed(slice: &str) -> bool {
    let bytes = slice.as_bytes();
    if bytes.is_empty() {
        return true;
    }
    if !slice.is_ascii() {
        return false;
    }
    if bytes[0].is_ascii_whitespace() || bytes[bytes.len() - 1].is_ascii_whitespace() {
        return false;
    }
    let mut prev_ws = false;
    for &b in bytes {
        if b.is_ascii_whitespace() {
            if b != b' ' || prev_ws {
                return false;
            }
            prev_ws = true;
        } else {
            prev_ws = false;
        }
    }
    true
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

    for cap in m.captures() {
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

    let impl_type = impl_type.or_else(|| {
        let anchor = range_node.or(trait_node)?;
        implementor_from_ancestor(anchor, source)
    })?;

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

    let mut current = node;
    for _ in 0..8 {
        let parent = current.parent()?;
        if let Some(text) = field_text(parent, "name", source) {
            return Some(text.to_string());
        }
        if let Some(type_node) = parent.child_by_field_name("type") {
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

    for cap in m.captures() {
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
    use crate::extract::RationaleKind;

    #[test]
    fn extract_implementation_rust_trait_impl() {
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
        assert!(!impls.is_empty(), "expected at least one Implementation; got none");
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
    fn python_signature_stops_at_the_body_instead_of_the_first_brace() {
        let src = b"def handler(context, key):\n    data = {\"a\": 1}\n    return data\n";
        let map = extract_l1("python", src).expect("extract");
        let sig = map
            .symbols
            .iter()
            .find(|s| s.name == "handler")
            .and_then(|s| s.signature.as_deref())
            .expect("handler signature");
        assert_eq!(sig, "def handler(context, key):");
    }

    #[test]
    fn extract_recovers_from_syntax_errors() {
        let src = br#"
pub fn good_one() {}

pub fn broken( {
    let x = ;
}

pub fn good_two() {}
"#;
        let map = extract_l1("rust", src).expect("extract should not fail on partial parse");
        assert!(map.had_errors, "had_errors should be true for syntax errors");
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

    #[test]
    fn extract_rationale_markers_and_citations_from_rust_comments() {
        let src = br#"
// WHY: keep the lock tight
pub fn locked() {}

// TODO: refactor
pub fn old() {}

pub fn danger() {
    // SAFETY: ptr is valid
    let _x = 1;
}

// see ADR-1 and RFC-2119
pub fn cited() {}
"#;
        let map = extract_l1("rust", src).expect("extract");
        let rationale = &map.rationale;
        assert_eq!(rationale.len(), 4, "expected 4 rationale records; got {rationale:?}");

        let why = rationale
            .iter()
            .find(|r| r.kind == RationaleKind::Why)
            .expect("why record");
        assert_eq!(why.text, "keep the lock tight");
        assert!(why.citations.is_empty());

        let todo = rationale
            .iter()
            .find(|r| r.kind == RationaleKind::Todo)
            .expect("todo record");
        assert_eq!(todo.text, "refactor");

        let safety = rationale
            .iter()
            .find(|r| r.kind == RationaleKind::Safety)
            .expect("safety record");
        assert_eq!(safety.text, "ptr is valid");

        let cited = rationale
            .iter()
            .find(|r| r.citations.contains(&"ADR-0001".to_string()))
            .expect("cited record");
        assert_eq!(cited.kind, RationaleKind::Note);
        assert_eq!(cited.text, "see ADR-1 and RFC-2119");
        assert_eq!(cited.citations, vec!["ADR-0001".to_string(), "RFC-2119".to_string()]);
    }

    #[test]
    fn extract_rationale_from_block_comment_rust() {
        let src = br#"
/**
 * WHY: keep the lock tight; see ADR-0001
 */
pub fn locked() {}
"#;
        let map = extract_l1("rust", src).expect("extract");
        let rationale = &map.rationale;
        let record = rationale
            .iter()
            .find(|r| r.kind == RationaleKind::Why)
            .expect("expected a WHY record from the block comment");
        assert_eq!(record.text, "keep the lock tight; see ADR-0001");
        assert_eq!(record.citations, vec!["ADR-0001".to_string()]);
    }

    #[test]
    fn extract_symbol_when_tslp_pattern_leads_with_doc_capture() {
        let src = b"# adds two numbers\ndef add(a, b)\n  a + b\nend\n";
        let Ok(map) = extract_l1("ruby", src) else {
            return;
        };
        let names: Vec<&str> = map.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"add"),
            "the commented Ruby method `add` must be extracted despite the leading @doc capture; got {names:?}"
        );
    }

    fn names_of(map: &FileMapL1) -> Vec<&str> {
        map.symbols.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn python_function_locals_are_not_symbols() {
        let src = b"LIMIT = 3\n\nclass A:\n    field = 1\n    def m(self):\n        inner = 2\n        return inner\n\ndef f():\n    local = 1\n    def nested():\n        pass\n    return local\n";
        let map = extract_l1("python", src).expect("extract");
        let names = names_of(&map);
        for want in ["LIMIT", "A", "field", "m", "f"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for bad in ["inner", "local", "nested"] {
            assert!(!names.contains(&bad), "{bad} must be excluded: {names:?}");
        }
    }

    #[test]
    fn javascript_function_locals_are_not_symbols() {
        let src = b"const TOP = 1;\nfunction f() { const a = 1; function g() {} const h = () => 1; return a; }\nclass C { m() { const b = 2; return b; } }\nconst arrow = () => { const c = 3; return c; };\n";
        let map = extract_l1("javascript", src).expect("extract");
        let names = names_of(&map);
        for want in ["TOP", "f", "C", "m", "arrow"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for bad in ["a", "g", "h", "b", "c"] {
            assert!(!names.contains(&bad), "{bad} must be excluded: {names:?}");
        }
    }

    #[test]
    fn typescript_function_locals_are_not_symbols() {
        let src = b"export const TOP: number = 1;\nexport function f(): void { const a = 1; }\nexport class C { x = 1; m(): void { const b = 2; } }\n";
        let map = extract_l1("typescript", src).expect("extract");
        let names = names_of(&map);
        for want in ["TOP", "f", "C", "m"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for bad in ["a", "b"] {
            assert!(!names.contains(&bad), "{bad} must be excluded: {names:?}");
        }
    }

    #[test]
    fn rust_function_locals_are_not_symbols() {
        let src = b"const TOP: u32 = 1;\nstruct S { f: u32 }\nimpl S { fn m(&self) { const LOCAL: u32 = 2; fn inner() {} } }\nfn free() { let x = 1; fn nested() {} }\n";
        let map = extract_l1("rust", src).expect("extract");
        let names = names_of(&map);
        for want in ["TOP", "S", "m", "free"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for bad in ["LOCAL", "inner", "x", "nested"] {
            assert!(!names.contains(&bad), "{bad} must be excluded: {names:?}");
        }
    }

    #[test]
    fn go_function_locals_are_not_symbols() {
        let src = b"package p\n\nvar Top = 1\n\nfunc F() {\n\tvar local = 2\n\t_ = local\n}\n\nfunc (s S) M() {\n\tconst c = 3\n}\n\ntype S struct{}\n";
        let map = extract_l1("go", src).expect("extract");
        let names = names_of(&map);
        for want in ["Top", "F", "M", "S"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for bad in ["local", "c"] {
            assert!(!names.contains(&bad), "{bad} must be excluded: {names:?}");
        }
    }

    #[test]
    fn java_function_locals_are_not_symbols() {
        let src = b"class A {\n  int field = 1;\n  void m() { int local = 2; }\n}\n";
        let Ok(map) = extract_l1("java", src) else { return };
        let names = names_of(&map);
        assert!(names.contains(&"A") && names.contains(&"m"), "{names:?}");
        assert!(!names.contains(&"local"), "{names:?}");
    }

    #[test]
    fn c_function_locals_are_not_symbols() {
        let src = b"int top = 1;\nint f(void) { int local = 2; return local; }\n";
        let Ok(map) = extract_l1("c", src) else { return };
        let names = names_of(&map);
        if names.is_empty() {
            return; // no symbol query for this grammar in this environment
        }
        assert!(names.contains(&"f"), "{names:?}");
        assert!(!names.contains(&"local"), "{names:?}");
    }

    #[test]
    fn value_signatures_drop_the_repeated_name_and_cap_length() {
        let long = "x".repeat(300);
        let src = format!("MAX_RETRIES = 3\nBIG = make({long})\n");
        let map = extract_l1("python", src.as_bytes()).expect("extract");
        let sig = |n: &str| {
            map.symbols
                .iter()
                .find(|s| s.name == n)
                .and_then(|s| s.signature.clone())
                .unwrap_or_default()
        };
        assert_eq!(sig("MAX_RETRIES"), "= 3");
        assert!(
            sig("BIG").chars().count() <= VALUE_SIGNATURE_MAX_CHARS + 1,
            "{}",
            sig("BIG")
        );
    }
}
