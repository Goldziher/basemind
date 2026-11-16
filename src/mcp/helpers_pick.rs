//! Choosing one definition out of a file's outline, for the tools that pin a symbol by name:
//! `code_refs` with `path`, `code_read`, `git_blame` and `git_log` with `symbol`.
//!
//! A name is not unique within a file (`new` in two `impl` blocks, `fmt` in two trait impls), and
//! taking the first match silently answers about the wrong definition. The caller disambiguates
//! with `line`: any 1-based line inside the definition it means. Without one, a definition at item
//! level wins over same-named functions nested in function bodies (a Rust `fn` inside a test).

use rmcp::ErrorData as McpError;

use super::helpers::kind_to_str;
use crate::extract::{Symbol, SymbolKind};
use crate::path::RelPath;

/// Definitions listed in a "not found" or "ambiguous" error before the rest are summarised.
const CANDIDATES_MAX: usize = 20;

/// The definition a pinned call names.
pub(super) struct Picked<'a> {
    pub symbol: &'a Symbol,
    /// Position among the same-named definitions of the file with the same nesting, in outline
    /// order, with [`NESTED_OCCURRENCE`] set for a nested one. `git_log` uses it to follow the same
    /// definition through revisions where the line numbers differ, so adding a nested function of
    /// the same name does not shift which item-level definition it follows.
    pub occurrence: usize,
}

/// Set in [`Picked::occurrence`] when the definition is nested in a function body.
pub(super) const NESTED_OCCURRENCE: usize = 1 << (usize::BITS - 1);

/// Whether `symbol` lies inside the span of another function or method of `symbols`.
pub(super) fn is_nested(symbols: &[Symbol], symbol: &Symbol) -> bool {
    symbols.iter().any(|outer| {
        matches!(outer.kind, SymbolKind::Function | SymbolKind::Method)
            && outer.start_byte <= symbol.start_byte
            && symbol.end_byte <= outer.end_byte
            && (outer.start_byte, outer.end_byte) != (symbol.start_byte, symbol.end_byte)
    })
}

/// [`Picked::occurrence`] of `picked` among `candidates`, the same-named definitions of `symbols`.
fn occurrence_of(symbols: &[Symbol], candidates: &[&Symbol], picked: &Symbol) -> usize {
    let nested = is_nested(symbols, picked);
    let index = candidates
        .iter()
        .filter(|c| is_nested(symbols, c) == nested)
        .position(|c| std::ptr::eq(*c, picked))
        .unwrap_or(0);
    if nested { index | NESTED_OCCURRENCE } else { index }
}

/// The definition `occurrence` names among the same-named definitions of `symbols`.
pub(super) fn nth_occurrence<'a>(
    symbols: &'a [Symbol],
    same_name: impl Iterator<Item = &'a Symbol>,
    occurrence: usize,
) -> Option<&'a Symbol> {
    let nested = occurrence & NESTED_OCCURRENCE != 0;
    let index = occurrence & !NESTED_OCCURRENCE;
    same_name.filter(|s| is_nested(symbols, s) == nested).nth(index)
}

/// 1-based inclusive line span of `symbol`. Without the file's bytes only the first line is known.
pub(super) fn line_span(symbol: &Symbol, source: Option<&[u8]>) -> (u32, u32) {
    let line = symbol.start_row + 1;
    let Some(bytes) = source else {
        return (line, line);
    };
    let start = (symbol.start_byte as usize).min(bytes.len());
    let end = (symbol.end_byte as usize).min(bytes.len()).max(start);
    let newlines = memchr::memchr_iter(b'\n', &bytes[start..end]).count() as u32;
    (line, line + newlines)
}

fn describe_span((line, end_line): (u32, u32)) -> String {
    if end_line > line {
        format!("lines {line}-{end_line}")
    } else {
        format!("line {line}")
    }
}

/// `items` joined, cut at [`CANDIDATES_MAX`] with a count of what was left out.
fn bounded_list(items: impl ExactSizeIterator<Item = String>) -> String {
    let total = items.len();
    let mut shown: Vec<String> = items.take(CANDIDATES_MAX).collect();
    if total > CANDIDATES_MAX {
        shown.push(format!("and {} more", total - CANDIDATES_MAX));
    }
    shown.join(", ")
}

/// Pick the definition of `name` in `symbols`, the outline of `path`.
///
/// `source` is the bytes the outline describes, when they could be read; it turns start rows into
/// line spans. `rev` is the revision the outline was taken at, for a tool that reads history: an
/// error then says where the symbol was looked for, since the working tree may well define it.
/// Errors name every candidate with its span, so the next call can pass a `line`.
#[allow(clippy::too_many_arguments)]
pub(super) fn pick_symbol<'a>(
    tool: &str,
    path: &RelPath,
    rev: Option<&str>,
    symbols: &'a [Symbol],
    source: Option<&[u8]>,
    name: &str,
    kind: Option<SymbolKind>,
    line: Option<u32>,
) -> Result<Picked<'a>, McpError> {
    let candidates: Vec<&Symbol> = symbols
        .iter()
        .filter(|symbol| symbol.name == name && kind.is_none_or(|wanted| symbol.kind == wanted))
        .collect();
    let describe = |symbol: &Symbol| {
        format!(
            "{} {} ({})",
            kind_to_str(symbol.kind),
            symbol.name,
            describe_span(line_span(symbol, source))
        )
    };
    let listed = || bounded_list(candidates.iter().map(|symbol| describe(symbol)));
    let place = match rev {
        Some(rev) => format!("`{path}` at {rev}"),
        None => format!("`{path}`"),
    };

    if candidates.is_empty() {
        let of_kind = kind
            .map(|kind| format!(" of kind `{}`", kind_to_str(kind)))
            .unwrap_or_default();
        let others = symbols
            .iter()
            .filter(|symbol| kind.is_none_or(|wanted| symbol.kind == wanted))
            .map(|symbol| format!("{} ({})", symbol.name, describe_span(line_span(symbol, source))))
            .collect::<Vec<_>>();
        let available = if others.is_empty() {
            "the file has no such symbols".to_string()
        } else {
            format!("it defines: {}", bounded_list(others.into_iter()))
        };
        return Err(McpError::invalid_params(
            format!("`{tool}`: no symbol `{name}`{of_kind} in {place}; {available}"),
            None,
        ));
    }

    let Some(line) = line else {
        let mut item_level = candidates.iter().filter(|c| !is_nested(symbols, c));
        let only = match (candidates.len(), item_level.next(), item_level.next()) {
            (1, _, _) => Some(candidates[0]),
            (_, Some(first), None) => Some(*first),
            _ => None,
        };
        if let Some(symbol) = only {
            return Ok(Picked {
                symbol,
                occurrence: occurrence_of(symbols, &candidates, symbol),
            });
        }
        return Err(McpError::invalid_params(
            format!(
                "`{tool}`: `{name}` names {} definitions in {place}: {}. Pass `line` (any line inside the one \
                 you mean).",
                candidates.len(),
                listed()
            ),
            None,
        ));
    };

    let holding = candidates
        .iter()
        .filter(|symbol| {
            let (first, last) = line_span(symbol, source);
            (first..=last).contains(&line)
        })
        .min_by_key(|symbol| symbol.end_byte.saturating_sub(symbol.start_byte));
    match holding {
        Some(symbol) => Ok(Picked {
            symbol,
            occurrence: occurrence_of(symbols, &candidates, symbol),
        }),
        None => Err(McpError::invalid_params(
            format!(
                "`{tool}`: `line` {line} is inside no definition of `{name}` in {place}: {}",
                listed()
            ),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &[u8] = b"fn new() {\n}\n\nfn new() {\n    1\n}\nfn other() {}\n";

    fn symbol(name: &str, start_row: u32, start_byte: u32, end_byte: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            start_byte,
            end_byte,
            start_row,
            start_col: 0,
            signature: None,
            decorators: Vec::new(),
        }
    }

    fn outline() -> Vec<Symbol> {
        vec![
            symbol("new", 0, 0, 12),
            symbol("new", 3, 14, 32),
            symbol("other", 6, 33, 46),
        ]
    }

    fn message(result: Result<Picked<'_>, McpError>) -> String {
        match result {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.message.into_owned(),
        }
    }

    #[test]
    fn a_unique_name_needs_no_line() {
        let symbols = outline();
        let path = RelPath::from("a.rs");
        let picked = pick_symbol("t", &path, None, &symbols, Some(SOURCE), "other", None, None).expect("unique");
        assert_eq!(picked.symbol.start_row, 6);
        assert_eq!(picked.occurrence, 0);
    }

    #[test]
    fn an_ambiguous_name_lists_every_candidate_with_its_span() {
        let symbols = outline();
        let path = RelPath::from("a.rs");
        let text = message(pick_symbol("t", &path, None, &symbols, Some(SOURCE), "new", None, None));
        assert!(text.contains("function new (lines 1-2)"), "{text}");
        assert!(text.contains("function new (lines 4-6)"), "{text}");
        assert!(text.contains("Pass `line`"), "{text}");
    }

    #[test]
    fn a_line_inside_a_definition_picks_it_and_reports_its_occurrence() {
        let symbols = outline();
        let path = RelPath::from("a.rs");
        let picked = pick_symbol("t", &path, None, &symbols, Some(SOURCE), "new", None, Some(5)).expect("second");
        assert_eq!(picked.symbol.start_row, 3);
        assert_eq!(picked.occurrence, 1);
        let text = message(pick_symbol(
            "t",
            &path,
            None,
            &symbols,
            Some(SOURCE),
            "new",
            None,
            Some(7),
        ));
        assert!(text.contains("inside no definition"), "{text}");
    }

    #[test]
    fn an_error_about_a_revision_names_it() {
        let symbols = outline();
        let path = RelPath::from("a.rs");
        let at = Some("HEAD~2");
        let missing = message(pick_symbol("t", &path, at, &symbols, Some(SOURCE), "nope", None, None));
        assert!(missing.contains("no symbol `nope` in `a.rs` at HEAD~2;"), "{missing}");
        let ambiguous = message(pick_symbol("t", &path, at, &symbols, Some(SOURCE), "new", None, None));
        assert!(ambiguous.contains("2 definitions in `a.rs` at HEAD~2:"), "{ambiguous}");
        let outside = message(pick_symbol(
            "t",
            &path,
            at,
            &symbols,
            Some(SOURCE),
            "new",
            None,
            Some(7),
        ));
        assert!(outside.contains("of `new` in `a.rs` at HEAD~2:"), "{outside}");
    }

    /// `fn run() {}` at item level, then `fn outer() { fn run() {} }`.
    const NESTED: &[u8] = b"fn run() {}\nfn outer() {\n    fn run() {}\n}\n";

    fn nested_outline() -> Vec<Symbol> {
        let mut symbols = vec![
            symbol("run", 0, 0, 11),
            symbol("outer", 1, 12, 42),
            symbol("run", 2, 29, 40),
        ];
        symbols.sort_by_key(|s| s.start_byte);
        symbols
    }

    #[test]
    fn without_a_line_an_item_level_definition_wins_over_nested_ones() {
        let symbols = nested_outline();
        let path = RelPath::from("a.rs");
        let picked = pick_symbol("t", &path, None, &symbols, Some(NESTED), "run", None, None).expect("item level");
        assert_eq!(picked.symbol.start_row, 0);
        assert_eq!(picked.occurrence, 0);
    }

    #[test]
    fn a_nested_definition_counts_its_occurrence_among_nested_ones_only() {
        let symbols = nested_outline();
        let path = RelPath::from("a.rs");
        let picked = pick_symbol("t", &path, None, &symbols, Some(NESTED), "run", None, Some(3)).expect("nested");
        assert_eq!(picked.symbol.start_row, 2);
        assert_eq!(picked.occurrence, NESTED_OCCURRENCE);
        let same_name = || symbols.iter().filter(|s| s.name == "run");
        assert_eq!(
            nth_occurrence(&symbols, same_name(), picked.occurrence).map(|s| s.start_row),
            Some(2)
        );
        assert_eq!(nth_occurrence(&symbols, same_name(), 0).map(|s| s.start_row), Some(0));
    }

    #[test]
    fn a_nested_function_added_before_an_item_does_not_shift_its_occurrence() {
        // Revision 2 nests a `run` inside `outer` ahead of the item-level `run`.
        let v2: Vec<Symbol> = vec![
            symbol("outer", 0, 0, 30),
            symbol("run", 1, 13, 24),
            symbol("run", 3, 31, 42),
        ];
        let same_name = || v2.iter().filter(|s| s.name == "run");
        assert_eq!(nth_occurrence(&v2, same_name(), 0).map(|s| s.start_row), Some(3));
    }

    #[test]
    fn two_item_level_definitions_stay_ambiguous_without_a_line() {
        let mut symbols = outline();
        symbols.push(symbol("new", 7, 50, 55));
        let path = RelPath::from("a.rs");
        let text = message(pick_symbol("t", &path, None, &symbols, Some(SOURCE), "new", None, None));
        assert!(text.contains("names 3 definitions"), "{text}");
    }

    #[test]
    fn a_missing_name_lists_a_bounded_set_of_what_the_file_defines() {
        let symbols: Vec<Symbol> = (0..30).map(|row| symbol(&format!("f{row}"), row, 0, 0)).collect();
        let path = RelPath::from("a.rs");
        let text = message(pick_symbol("t", &path, None, &symbols, None, "nope", None, None));
        assert!(text.contains("no symbol `nope` in `a.rs`"), "{text}");
        assert!(text.contains("f0 (line 1)") && text.contains("f19 (line 20)"), "{text}");
        assert!(!text.contains("f20 ("), "{text}");
        assert!(text.contains("and 10 more"), "{text}");
    }
}
