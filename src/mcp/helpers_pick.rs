//! Choosing one definition out of a file's outline, for the tools that pin a symbol by name:
//! `code_refs` with `path`, `code_read`, `git_blame` and `git_log` with `symbol`.
//!
//! A name is not unique within a file (`new` in two `impl` blocks, `fmt` in two trait impls), and
//! taking the first match silently answers about the wrong definition. The caller disambiguates
//! with `line`: any 1-based line inside the definition it means.

use rmcp::ErrorData as McpError;

use super::helpers::kind_to_str;
use crate::extract::{Symbol, SymbolKind};
use crate::path::RelPath;

/// Definitions listed in a "not found" or "ambiguous" error before the rest are summarised.
const CANDIDATES_MAX: usize = 20;

/// The definition a pinned call names.
pub(super) struct Picked<'a> {
    pub symbol: &'a Symbol,
    /// Position among the same-named definitions of the file, in outline order. `git_log` uses it to
    /// follow the same definition through revisions where the line numbers differ.
    pub occurrence: usize,
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
/// `source` is the file's current bytes when they could be read; it turns start rows into line
/// spans. Errors name every candidate with its span, so the next call can pass a `line`.
pub(super) fn pick_symbol<'a>(
    tool: &str,
    path: &RelPath,
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
            format!("`{tool}`: no symbol `{name}`{of_kind} in `{path}`; {available}"),
            None,
        ));
    }

    let Some(line) = line else {
        if candidates.len() == 1 {
            return Ok(Picked {
                symbol: candidates[0],
                occurrence: 0,
            });
        }
        return Err(McpError::invalid_params(
            format!(
                "`{tool}`: `{name}` names {} definitions in `{path}`: {}. Pass `line` (any line inside the one \
                 you mean).",
                candidates.len(),
                listed()
            ),
            None,
        ));
    };

    let holding = candidates
        .iter()
        .enumerate()
        .filter(|(_, symbol)| {
            let (first, last) = line_span(symbol, source);
            (first..=last).contains(&line)
        })
        .min_by_key(|(_, symbol)| symbol.end_byte.saturating_sub(symbol.start_byte));
    match holding {
        Some((occurrence, symbol)) => Ok(Picked { symbol, occurrence }),
        None => Err(McpError::invalid_params(
            format!(
                "`{tool}`: `line` {line} is inside no definition of `{name}` in `{path}`: {}",
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
        let picked = pick_symbol("t", &path, &symbols, Some(SOURCE), "other", None, None).expect("unique");
        assert_eq!(picked.symbol.start_row, 6);
        assert_eq!(picked.occurrence, 0);
    }

    #[test]
    fn an_ambiguous_name_lists_every_candidate_with_its_span() {
        let symbols = outline();
        let path = RelPath::from("a.rs");
        let text = message(pick_symbol("t", &path, &symbols, Some(SOURCE), "new", None, None));
        assert!(text.contains("function new (lines 1-2)"), "{text}");
        assert!(text.contains("function new (lines 4-6)"), "{text}");
        assert!(text.contains("Pass `line`"), "{text}");
    }

    #[test]
    fn a_line_inside_a_definition_picks_it_and_reports_its_occurrence() {
        let symbols = outline();
        let path = RelPath::from("a.rs");
        let picked = pick_symbol("t", &path, &symbols, Some(SOURCE), "new", None, Some(5)).expect("second");
        assert_eq!(picked.symbol.start_row, 3);
        assert_eq!(picked.occurrence, 1);
        let text = message(pick_symbol("t", &path, &symbols, Some(SOURCE), "new", None, Some(7)));
        assert!(text.contains("inside no definition"), "{text}");
    }

    #[test]
    fn a_missing_name_lists_a_bounded_set_of_what_the_file_defines() {
        let symbols: Vec<Symbol> = (0..30).map(|row| symbol(&format!("f{row}"), row, 0, 0)).collect();
        let path = RelPath::from("a.rs");
        let text = message(pick_symbol("t", &path, &symbols, None, "nope", None, None));
        assert!(text.contains("no symbol `nope` in `a.rs`"), "{text}");
        assert!(text.contains("f0 (line 1)") && text.contains("f19 (line 20)"), "{text}");
        assert!(!text.contains("f20 ("), "{text}");
        assert!(text.contains("and 10 more"), "{text}");
    }
}
