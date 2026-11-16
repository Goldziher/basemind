//! Item-position Rust macro invocations (`cfg_time! { pub fn f() {} }`), parsed as Rust.
//!
//! tree-sitter-rust leaves every macro body as an opaque `token_tree`, so a crate that gates whole
//! modules behind `cfg_*!` wrappers (tokio) loses those definitions and the calls inside them. A body
//! in item position is re-parsed on its own with the pooled parser, once per file: L1 reads its
//! symbols, imports and impls, L2 runs the real calls query over it, and both shift what they find
//! back to the file's coordinates. Bodies that do not parse cleanly as Rust (`cfg_if!`) contribute
//! no items, and their calls stay with L2's lexical token-tree match.

use tree_sitter::{Node, Point, Tree};

use super::l1::{CombinedL1, run_combined};
use super::{Call, ExtractError, Implementation, Import, Symbol};
use crate::lang::{LangId, ParseOutcome, parse_with_default_timeout, with_parser};

/// Largest macro body re-parsed; a bigger one is left opaque.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// How many macro layers deep (`cfg_rt! { cfg_net! { … } }`) bodies are opened.
const MAX_DEPTH: u8 = 3;
/// Bodies re-parsed per file, across every depth.
const MAX_BODIES_PER_FILE: usize = 256;

/// First tokens a body of items can start with. Anything else (`if #[cfg(unix)]` in `cfg_if!`, a
/// `key => value` table) is not worth a parse that would fail.
const ITEM_START_TOKENS: &[&[u8]] = &[
    b"pub",
    b"fn",
    b"struct",
    b"enum",
    b"union",
    b"trait",
    b"impl",
    b"type",
    b"const",
    b"static",
    b"mod",
    b"use",
    b"extern",
    b"unsafe",
    b"async",
    b"macro_rules",
];

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

/// One macro body that parsed cleanly as Rust.
struct MacroBody {
    tree: Tree,
    /// File position of the body's first byte (just inside the opening delimiter).
    origin: Origin,
    len: usize,
    /// Macro layers around the body: 1 for a body found in the file's own tree.
    depth: u8,
}

impl MacroBody {
    fn text<'s>(&self, source: &'s [u8]) -> &'s [u8] {
        &source[self.origin.byte..self.origin.byte + self.len]
    }
}

/// The cleanly parsed item-position macro bodies of one file, in file order. Built once per file
/// and shared by L1 and L2, so each body is parsed once whichever tiers are extracted.
#[derive(Default)]
pub(super) struct MacroBodies {
    bodies: Vec<MacroBody>,
}

impl MacroBodies {
    /// Parse the item-position macro bodies under `root`. Empty for every language but Rust.
    pub(super) fn collect(lang: LangId, root: Node, source: &[u8]) -> Result<Self, ExtractError> {
        let mut out = Self::default();
        if lang != "rust" {
            return Ok(out);
        }
        let mut budget = MAX_BODIES_PER_FILE;
        let base = Origin {
            byte: 0,
            point: Point { row: 0, column: 0 },
        };
        collect(lang, root, source, base, 0, &mut budget, &mut out.bodies)?;
        out.bodies.sort_by_key(|b| b.origin.byte);
        Ok(out)
    }

    /// True when byte `at` of the file lies inside a parsed body more than `depth` macro layers
    /// deep: a token tree there belongs to that body's own parse, not to the tree at `depth`.
    pub(super) fn covers(&self, at: usize, depth: u8) -> bool {
        let end = self.bodies.partition_point(|b| b.origin.byte <= at);
        self.bodies[..end]
            .iter()
            .any(|b| b.depth > depth && at < b.origin.byte + b.len)
    }

    /// Append the symbols, imports and trait impls declared inside the bodies, in file coordinates.
    pub(super) fn extend_l1(&self, lang: LangId, source: &[u8], out: &mut CombinedL1) -> Result<(), ExtractError> {
        if self.bodies.is_empty() {
            return Ok(());
        }
        let before = (out.0.len(), out.1.len(), out.2.len());
        for body in &self.bodies {
            let (symbols, imports, impls) = run_combined(lang, body.tree.root_node(), body.text(source))?;
            let o = body.origin;
            out.0.extend(symbols.into_iter().map(|s| shift_symbol(s, o)));
            out.1.extend(imports.into_iter().map(|i| shift_import(i, o)));
            out.2.extend(impls.into_iter().map(|i| shift_impl(i, o)));
        }
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

    /// Append the calls inside the bodies, in file coordinates. `run_calls(root, text, skip_lexical)`
    /// extracts one tree's calls; `skip_lexical` takes a byte of `text` and says whether a token tree
    /// there belongs to a deeper parsed body.
    pub(super) fn extend_calls<F>(
        &self,
        source: &[u8],
        out: &mut Vec<Call>,
        mut run_calls: F,
    ) -> Result<(), ExtractError>
    where
        F: FnMut(Node, &[u8], &dyn Fn(usize) -> bool) -> Result<Vec<Call>, ExtractError>,
    {
        if self.bodies.is_empty() {
            return Ok(());
        }
        for body in &self.bodies {
            let skip = |at: usize| self.covers(body.origin.byte + at, body.depth);
            let calls = run_calls(body.tree.root_node(), body.text(source), &skip)?;
            let o = body.origin;
            out.extend(calls.into_iter().map(|mut c| {
                c.start_byte = o.shift_byte(c.start_byte);
                c.end_byte = o.shift_byte(c.end_byte);
                (c.start_row, c.start_col) = o.shift_point(c.start_row, c.start_col);
                c
            }));
        }
        out.sort_by_key(|c| c.start_byte);
        Ok(())
    }
}

fn collect(
    lang: LangId,
    root: Node,
    source: &[u8],
    origin: Origin,
    depth: u8,
    budget: &mut usize,
    out: &mut Vec<MacroBody>,
) -> Result<(), ExtractError> {
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    for body in item_macro_bodies(root, source) {
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
        if tree.root_node().has_error() {
            continue;
        }
        let open = body.start_position();
        let (row, column) = origin.shift_point(open.row as u32, open.column as u32 + 1);
        let inner = Origin {
            byte: origin.byte + start,
            point: Point {
                row: row as usize,
                column: column as usize,
            },
        };
        collect(lang, tree.root_node(), text, inner, depth + 1, budget, out)?;
        out.push(MacroBody {
            tree,
            origin: inner,
            len: end - start,
            depth: depth + 1,
        });
    }
    Ok(())
}

/// The `token_tree` bodies of macro invocations that stand where an item may — at file level and in
/// `mod`, `impl`, `trait` and `extern` bodies — and open like a list of items. Expression macros
/// inside function bodies are not visited, so the walk touches item nodes only.
fn item_macro_bodies<'t>(root: Node<'t>, source: &[u8]) -> Vec<Node<'t>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    let mut cursor = root.walk();
    while let Some(container) = stack.pop() {
        for child in container.named_children(&mut cursor) {
            match child.kind() {
                "macro_invocation" => {
                    // The body is the invocation's last child; no field names it.
                    if let Some(tt) = child
                        .child(child.child_count().saturating_sub(1))
                        .filter(|n| n.kind() == "token_tree" && opens_like_items(*n, source))
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
    found
}

/// Whether the first token inside `tt` (comments skipped) can start an item: an attribute, an item
/// keyword, or a nested macro invocation (`name!`, `path::name!`).
fn opens_like_items(tt: Node, source: &[u8]) -> bool {
    let text = |n: Node| &source[n.start_byte()..n.end_byte()];
    let mut i = 1;
    let first = loop {
        match tt.child(i) {
            Some(n) if n.is_extra() => i += 1,
            Some(n) if n.end_byte() < tt.end_byte() => break n,
            _ => return false,
        }
    };
    let t = text(first);
    if t.starts_with(b"#") || ITEM_START_TOKENS.contains(&t) {
        return true;
    }
    (first.kind() == "identifier" || matches!(t, b"crate" | b"self" | b"super"))
        && first
            .next_sibling()
            .is_some_and(|n| text(n).starts_with(b"!") || text(n).starts_with(b"::"))
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
