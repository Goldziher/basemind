//! Items declared inside item-position Rust macro invocations (`cfg_time! { pub fn f() {} }`).
//!
//! tree-sitter-rust leaves every macro body as an opaque `token_tree`, so a crate that gates whole
//! modules behind `cfg_*!` wrappers (tokio) loses those definitions. A body in item position is
//! re-parsed on its own with the pooled parser and its outline is shifted back to the file's
//! coordinates. Bodies that do not parse cleanly as Rust (`cfg_if!`, `lazy_static!`) contribute
//! nothing rather than guesses.

use tree_sitter::{Node, Point};

use super::l1::{CombinedL1, run_combined};
use super::{ExtractError, Implementation, Import, Symbol};
use crate::lang::{LangId, ParseOutcome, parse_with_default_timeout, with_parser};

/// Largest macro body re-parsed; a bigger one is left opaque.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// How many macro layers deep (`cfg_rt! { cfg_net! { … } }`) bodies are opened.
const MAX_DEPTH: u8 = 3;
/// Bodies re-parsed per file, across every depth.
const MAX_BODIES_PER_FILE: usize = 256;

/// Where a re-parsed body sits in the enclosing file.
#[derive(Clone, Copy)]
struct Origin {
    byte: usize,
    point: Point,
}

impl Origin {
    fn shift_byte(self, b: u32) -> u32 {
        b.saturating_add(self.byte as u32)
    }

    /// `(row, col)` of a body-relative position: only positions on the body's first line share the
    /// opening delimiter's line, so only they carry its column.
    fn shift_point(self, row: u32, col: u32) -> (u32, u32) {
        let col = if row == 0 { col + self.point.column as u32 } else { col };
        (row + self.point.row as u32, col)
    }
}

/// Append the symbols, imports and trait impls declared inside item-position macro bodies under
/// `root`, in file coordinates.
pub(super) fn extend_with_macro_items(
    lang: LangId,
    root: Node,
    source: &[u8],
    out: &mut CombinedL1,
) -> Result<(), ExtractError> {
    let mut budget = MAX_BODIES_PER_FILE;
    let base = Origin {
        byte: 0,
        point: Point { row: 0, column: 0 },
    };
    let before = (out.0.len(), out.1.len(), out.2.len());
    collect(lang, root, source, base, 0, &mut budget, out)?;
    if out.0.len() != before.0 {
        out.0.sort_by_key(|s| s.start_byte);
    }
    if out.1.len() != before.1 {
        out.1.sort_by_key(|i| i.start_byte);
    }
    if out.2.len() != before.2 {
        out.2.sort_by_key(|i| i.start_byte);
    }
    Ok(())
}

fn collect(
    lang: LangId,
    root: Node,
    source: &[u8],
    origin: Origin,
    depth: u8,
    budget: &mut usize,
    out: &mut CombinedL1,
) -> Result<(), ExtractError> {
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    for body in item_macro_bodies(root) {
        if *budget == 0 {
            return Ok(());
        }
        let (start, end) = (body.start_byte() + 1, body.end_byte().saturating_sub(1));
        if end <= start || end - start > MAX_BODY_BYTES {
            continue;
        }
        *budget -= 1;
        let text = &source[start..end];
        let tree = match with_parser(lang, |p| parse_with_default_timeout(p, text))? {
            ParseOutcome::Ok(tree) => tree,
            ParseOutcome::Failed | ParseOutcome::TimedOut => continue,
        };
        let sub_root = tree.root_node();
        if sub_root.has_error() {
            continue;
        }
        let open = body.start_position();
        let inner = Origin {
            byte: origin.byte + start,
            point: {
                let (row, column) = origin.shift_point(open.row as u32, open.column as u32 + 1);
                Point {
                    row: row as usize,
                    column: column as usize,
                }
            },
        };
        let (symbols, imports, impls) = run_combined(lang, sub_root, text)?;
        out.0.extend(symbols.into_iter().map(|s| shift_symbol(s, inner)));
        out.1.extend(imports.into_iter().map(|i| shift_import(i, inner)));
        out.2.extend(impls.into_iter().map(|i| shift_impl(i, inner)));
        collect(lang, sub_root, text, inner, depth + 1, budget, out)?;
    }
    Ok(())
}

/// The `token_tree` bodies of macro invocations that stand where an item may: at file level and in
/// `mod`, `impl`, `trait` and `extern` bodies. Expression macros inside function bodies are not
/// visited, so the walk touches item nodes only.
fn item_macro_bodies(root: Node) -> Vec<Node> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    let mut cursor = root.walk();
    while let Some(container) = stack.pop() {
        for child in container.named_children(&mut cursor) {
            match child.kind() {
                "macro_invocation" => {
                    if let Some(tt) = child
                        .named_children(&mut child.walk())
                        .find(|n| n.kind() == "token_tree")
                    {
                        found.push(tt);
                    }
                }
                "mod_item" | "impl_item" | "trait_item" | "foreign_mod_item" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        stack.push(body);
                    }
                }
                _ => {}
            }
        }
    }
    found.sort_by_key(|n| n.start_byte());
    found
}

fn shift_symbol(mut s: Symbol, o: Origin) -> Symbol {
    s.start_byte = o.shift_byte(s.start_byte);
    s.end_byte = o.shift_byte(s.end_byte);
    (s.start_row, s.start_col) = o.shift_point(s.start_row, s.start_col);
    s
}

fn shift_import(mut i: Import, o: Origin) -> Import {
    i.start_byte = o.shift_byte(i.start_byte);
    i.end_byte = o.shift_byte(i.end_byte);
    i
}

fn shift_impl(mut i: Implementation, o: Origin) -> Implementation {
    i.start_byte = o.shift_byte(i.start_byte);
    (i.start_row, i.start_col) = o.shift_point(i.start_row, i.start_col);
    i
}
