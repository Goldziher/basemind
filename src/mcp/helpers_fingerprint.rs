//! Structural-fingerprint helpers for `symbol_history`. Split out of
//! `helpers.rs` to keep both files under the 1000-line module cap. Everything here is `pub(super)`
//! or module-private, and re-exported from `helpers.rs` so existing call sites resolve unchanged.

use std::sync::Arc;

use rmcp::ErrorData as McpError;

use super::helpers::normalize_for_history;
use super::{OutlineCache, OutlineEntry};
use crate::extract::SymbolKind;
use crate::lang::{LangId, ParseOutcome, parse_with_default_timeout, with_parser};

/// `symbol_history` fingerprint mode. Picked per-call via the `hash_mode` request param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HashMode {
    /// Default. Byte-level fingerprint after `normalize_for_history` strips comments and
    /// collapses whitespace runs. Cheap, language-aware, but couples to string-literal
    /// whitespace (collapsing inside strings is documented as an accepted trade-off).
    Normalized,
    /// AST-shape fingerprint over the symbol's tree-sitter subtree. Formatter-stable,
    /// comment-stable. Includes literal *contents* so e.g. swapping a string-literal value
    /// still registers as a body change.
    Structural,
    /// Same as `Structural` but ignores literal contents — string/number leaves contribute
    /// only their node kind, not their text. Useful for "did the logic change, ignoring
    /// i18n / docstring churn" workflows. Will produce false negatives for changes that
    /// only modify literal values.
    StructuralLoose,
}

impl HashMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            HashMode::Normalized => "normalized",
            HashMode::Structural => "structural",
            HashMode::StructuralLoose => "structural_loose",
        }
    }
}

pub(super) fn parse_hash_mode(s: &str) -> Result<HashMode, McpError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "normalized" => HashMode::Normalized,
        "structural" => HashMode::Structural,
        "structural_loose" => HashMode::StructuralLoose,
        other => {
            return Err(McpError::invalid_params(
                format!("unknown hash_mode: {other} (expected normalized|structural|structural_loose)"),
                None,
            ));
        }
    })
}

/// Look up `(oid, lang)` in the outline cache; on miss, parse `source` via `extract_l1` and
/// insert a freshly built `OutlineEntry`. Returns the cached entry either way. Errors
/// surface as `None` so the caller can treat parse failures the same as "blob missing".
pub(super) fn outline_entry_for_blob(
    cache: &OutlineCache,
    oid: gix::ObjectId,
    lang: LangId,
    source: Vec<u8>,
) -> Option<Arc<OutlineEntry>> {
    let key = (oid, lang);
    if let Some(entry) = cache.get(&key) {
        return Some(entry);
    }
    let map = Arc::new(crate::extract::l1::extract_l1(lang, &source).ok()?);
    let entry = Arc::new(OutlineEntry {
        map,
        source: Arc::new(source),
    });
    cache.put(key, Arc::clone(&entry));
    Some(entry)
}

/// Compute a symbol-history fingerprint from an outline cache entry, choosing the strategy
/// based on `mode`. `occurrence` picks among same-named symbols of the file, as
/// [`Picked::occurrence`](super::helpers_pick::Picked) counts them. Returns the fingerprint as a `Vec<u8>` so the caller can compare
/// successive results with `==` regardless of which mode produced them.
pub(super) fn symbol_fingerprint(
    entry: &OutlineEntry,
    name: &str,
    kind: Option<SymbolKind>,
    occurrence: usize,
    lang: LangId,
    mode: HashMode,
) -> Option<Vec<u8>> {
    let symbols = &entry.map.symbols;
    let same_name = symbols
        .iter()
        .filter(|s| s.name == name && kind.is_none_or(|k| s.kind == k));
    let sym = super::helpers_pick::nth_occurrence(symbols, same_name, occurrence)?;
    let s = sym.start_byte as usize;
    let e = (sym.end_byte as usize).min(entry.source.len());
    if s >= e {
        return None;
    }
    match mode {
        HashMode::Normalized => Some(normalize_for_history(lang, &entry.source[s..e])),
        HashMode::Structural | HashMode::StructuralLoose => {
            let include_literals = matches!(mode, HashMode::Structural);
            structural_hash_of_symbol(lang, &entry.source, (s, e), include_literals).map(|h| h.to_vec())
        }
    }
}

/// AST-structural fingerprint for a symbol's subtree.
///
/// Re-parses `source` with tree-sitter, finds the node whose byte range matches `range`,
/// then DFS-walks the subtree feeding `(node_kind_name, identifier_or_literal_text)` pairs
/// into a blake3 hasher. Comments and other `is_extra()` nodes are skipped entirely,
/// non-named nodes (anonymous tokens like `{`, `(`) contribute nothing — this is what makes
/// the hash formatter-stable.
///
/// When `include_literals` is false, literal-leaf nodes (strings, numbers, booleans)
/// contribute only their kind name, not their text. Identifiers always contribute their
/// text — renaming a local variable always moves the hash.
fn structural_hash_of_symbol(
    lang: LangId,
    source: &[u8],
    range: (usize, usize),
    include_literals: bool,
) -> Option<[u8; 32]> {
    let tree = parse(lang, source)?;
    if let Some(node) = find_node_for_range(tree.root_node(), range.0, range.1) {
        return Some(hash_node(node, source, include_literals, lang));
    }
    // No node spans exactly the symbol when it was read out of a macro body, which the file's own
    // parse leaves as an opaque token tree. Parse the symbol's text alone and hash the item it
    // holds; hashing the smallest enclosing node would fingerprint the whole macro body instead.
    let text = &source[range.0..range.1];
    let sub = parse(lang, text)?;
    let root = sub.root_node();
    let mut cursor = root.walk();
    let item = root
        .named_children(&mut cursor)
        .find(|n| !n.is_extra())
        .filter(|_| !root.has_error());
    Some(match item {
        Some(item) => hash_node(item, text, include_literals, lang),
        None => *blake3::hash(&normalize_for_history(lang, text)).as_bytes(),
    })
}

fn parse(lang: LangId, source: &[u8]) -> Option<tree_sitter::Tree> {
    match with_parser(lang, |p| parse_with_default_timeout(p, source)).ok()? {
        ParseOutcome::Ok(t) => Some(t),
        _ => None,
    }
}

fn hash_node(node: tree_sitter::Node, source: &[u8], include_literals: bool, lang: LangId) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    walk_structural(node, source, include_literals, lang, &mut hasher);
    *hasher.finalize().as_bytes()
}

/// The node spanning exactly `start..end`, if the tree has one.
fn find_node_for_range(root: tree_sitter::Node, start: usize, end: usize) -> Option<tree_sitter::Node> {
    let mut node = root;
    let mut cursor = root.walk();
    loop {
        if node.start_byte() == start && node.end_byte() == end {
            return Some(node);
        }
        node = node
            .children(&mut cursor)
            .find(|c| c.start_byte() <= start && c.end_byte() >= end)?;
    }
}

fn walk_structural(
    node: tree_sitter::Node,
    source: &[u8],
    include_literals: bool,
    lang: LangId,
    hasher: &mut blake3::Hasher,
) {
    if node.is_extra() {
        return;
    }
    let kind_name = node.kind();
    hasher.update(&(kind_name.len() as u32).to_le_bytes());
    hasher.update(kind_name.as_bytes());

    let nc = node.named_child_count() as u32;
    let named_count: u32 = (0..nc)
        .filter(|&i| node.named_child(i).is_some_and(|c| !c.is_extra()))
        .count() as u32;

    if named_count == 0 {
        let emit_text = is_identifier_kind(kind_name) || (include_literals && is_literal_kind(lang, kind_name));
        if emit_text && let Ok(text) = node.utf8_text(source) {
            hasher.update(&(text.len() as u32).to_le_bytes());
            hasher.update(text.as_bytes());
        } else {
            hasher.update(&0u32.to_le_bytes());
        }
        return;
    }
    hasher.update(&named_count.to_le_bytes());
    for i in 0..nc {
        if let Some(child) = node.named_child(i)
            && !child.is_extra()
        {
            walk_structural(child, source, include_literals, lang, hasher);
        }
    }
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "property_identifier"
            | "type_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "field_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "namespace_identifier"
    )
}

fn is_literal_kind(lang: LangId, kind: &str) -> bool {
    if matches!(
        kind,
        "string"
            | "string_fragment"
            | "string_content"
            | "template_string"
            | "template_substitution"
            | "number"
            | "integer"
            | "float"
            | "true"
            | "false"
            | "null"
            | "none"
    ) {
        return true;
    }
    match lang {
        "rust" => matches!(
            kind,
            "char_literal"
                | "string_literal"
                | "byte_string_literal"
                | "raw_string_literal"
                | "integer_literal"
                | "float_literal"
                | "boolean_literal"
        ),
        "go" => matches!(
            kind,
            "interpreted_string_literal"
                | "raw_string_literal"
                | "rune_literal"
                | "int_literal"
                | "float_literal"
                | "imaginary_literal"
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(src: &str) -> OutlineEntry {
        OutlineEntry {
            map: Arc::new(crate::extract::l1::extract_l1("rust", src.as_bytes()).expect("extract")),
            source: Arc::new(src.as_bytes().to_vec()),
        }
    }

    fn fingerprint(src: &str, name: &str, mode: HashMode) -> Vec<u8> {
        symbol_fingerprint(&entry(src), name, None, 0, "rust", mode).expect("fingerprint")
    }

    #[test]
    fn a_symbol_inside_a_macro_body_is_fingerprinted_on_its_own() {
        let before = "cfg_x! {\n    fn a() { one() }\n    fn b() { two() }\n}\n";
        let b_changed = "cfg_x! {\n    fn a() { one() }\n    fn b() { three() }\n}\n";
        let a_changed = "cfg_x! {\n    fn a() { four() }\n    fn b() { two() }\n}\n";
        for mode in [HashMode::Structural, HashMode::StructuralLoose] {
            let a = fingerprint(before, "a", mode);
            assert_ne!(a, fingerprint(before, "b", mode), "a and b hash the whole macro body");
            assert_eq!(a, fingerprint(b_changed, "a", mode), "a sibling's edit moved a's hash");
            assert_ne!(
                a,
                fingerprint(a_changed, "a", mode),
                "a's own edit did not move its hash"
            );
        }
    }

    #[test]
    fn a_macro_body_symbol_hashes_like_the_same_item_written_at_file_level() {
        let in_macro = "cfg_x! {\n    fn a() { one(1) }\n}\n";
        let top = "fn a() { one(1) }\n";
        assert_eq!(
            fingerprint(in_macro, "a", HashMode::Structural),
            fingerprint(top, "a", HashMode::Structural)
        );
    }
}
