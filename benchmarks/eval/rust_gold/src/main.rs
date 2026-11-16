//! Rust ground truth for the basemind eval, parsed with `syn` rather than tree-sitter, so the gold
//! does not share basemind's parser or its queries.
//!
//! Input (stdin), repeated per file: `<name>\n<byte length>\n<bytes>`. `<name>` is echoed back as
//! `path` (a generator may pass `<sha>:<path>` to extract an older revision).
//!
//! Output (stdout): one JSON object per line, `t` naming the record:
//!
//! - `def`: a definition. `line` is the first token after the item's attributes and doc comments,
//!   `name_line` the line of its name, `end` the line of its last token. `kind` is one of `fn`,
//!   `method` (a fn in an impl or trait with a body), `method_decl` (a trait fn without a body),
//!   `struct`, `enum`, `union`, `trait`, `type`, `assoc_type_decl`, `const`, `assoc_const_decl`,
//!   `static`, `mod`, `macro` (`macro_rules!`), `impl` (named after the implementing type),
//!   `foreign_fn`, `foreign_static`, `foreign_type`. `body` is the item's tokens without attributes
//!   (whitespace- and comment-insensitive), for comparing a symbol across revisions.
//! - `call`: a call site. `name` is the callee's last path segment, the method name, or the macro
//!   name; `line` is the line of that identifier; `via` is `call`, `method` or `macro`.
//! - `impl`: a trait implementation, `trait` and `self_ty` by last path segment.
//! - `use`: one leaf of a `use` tree as a full path (`module`, e.g. `crate::rt::spawn`).
//! - `opaque`: a macro invocation whose tokens parse neither as items, statements nor
//!   comma-separated expressions. Calls inside it are found lexically (`ident(`, `ident!(`);
//!   definitions inside it are not reported.
//! - `error`: the file did not parse.
//!
//! Macro invocation bodies are re-parsed (items, then statements, then expressions), so a call
//! inside `assert_eq!(f(), 1)` and a definition inside `cfg_rt! { pub fn spawn() {} }` are
//! reported. `macro_rules!` bodies are patterns, not code, and are never descended into.

use std::fmt::Write as _;
use std::io::{BufRead, Read, Write};

use proc_macro2::{Delimiter, Ident, Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Expr, Token};

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// One output record: an ordered list of `(key, value)` where a value is a string or a number.
enum V<'a> {
    S(&'a str),
    N(usize),
}

fn record(out: &mut String, fields: &[(&str, V<'_>)]) {
    out.push('{');
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_str(out, k);
        out.push(':');
        match v {
            V::S(s) => json_str(out, s),
            V::N(n) => {
                let _ = write!(out, "{n}");
            }
        }
    }
    out.push_str("}\n");
}

/// Tokens of `node` with leading outer attributes (`#[...]`, doc comments included) removed.
fn without_attrs(node: &impl ToTokens) -> Vec<TokenTree> {
    let tokens: Vec<TokenTree> = node.to_token_stream().into_iter().collect();
    let mut i = 0;
    while i + 1 < tokens.len() {
        match (&tokens[i], &tokens[i + 1]) {
            (TokenTree::Punct(p), TokenTree::Group(g)) if p.as_char() == '#' && g.delimiter() == Delimiter::Bracket => {
                i += 2
            }
            _ => break,
        }
    }
    tokens[i..].to_vec()
}

fn last_line(tokens: &[TokenTree]) -> usize {
    tokens.last().map_or(0, |t| t.span().end().line)
}

fn line(span: Span) -> usize {
    span.start().line
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map_or_else(|| ty.to_token_stream().to_string(), |s| s.ident.to_string()),
        syn::Type::Reference(r) => type_name(&r.elem),
        syn::Type::Paren(p) => type_name(&p.elem),
        syn::Type::Group(g) => type_name(&g.elem),
        other => other.to_token_stream().to_string(),
    }
}

/// Every leaf of a `use` tree as a full `a::b::c` path (a rename keeps the original name, a glob
/// ends in `*`).
fn use_paths(tree: &syn::UseTree, prefix: String, out: &mut Vec<String>) {
    let join = |s: &str| {
        if prefix.is_empty() {
            s.to_string()
        } else {
            format!("{prefix}::{s}")
        }
    };
    match tree {
        syn::UseTree::Path(p) => use_paths(&p.tree, join(&p.ident.to_string()), out),
        syn::UseTree::Name(n) => out.push(join(&n.ident.to_string())),
        syn::UseTree::Rename(r) => out.push(join(&r.ident.to_string())),
        syn::UseTree::Glob(_) => out.push(join("*")),
        syn::UseTree::Group(g) => g.items.iter().for_each(|t| use_paths(t, prefix.clone(), out)),
    }
}

struct Extractor<'a> {
    path: &'a str,
    out: String,
    /// Inside an `impl` or `trait` body: a fn there is a method.
    in_assoc: usize,
}

impl Extractor<'_> {
    fn def(&mut self, node: &impl ToTokens, name: &str, name_span: Span, kind: &str) {
        let tokens = without_attrs(node);
        let first = tokens.first().map_or(line(name_span), |t| line(t.span()));
        // A module or impl body is the whole file again; nothing compares those across revisions.
        let body = if matches!(kind, "mod" | "impl") {
            String::new()
        } else {
            tokens.iter().cloned().collect::<TokenStream>().to_string()
        };
        let path = self.path;
        record(
            &mut self.out,
            &[
                ("t", V::S("def")),
                ("path", V::S(path)),
                ("name", V::S(name)),
                ("kind", V::S(kind)),
                ("line", V::N(first)),
                ("name_line", V::N(line(name_span))),
                ("end", V::N(last_line(&tokens).max(first))),
                ("body", V::S(&body)),
            ],
        );
    }

    fn call(&mut self, ident: &Ident, via: &str) {
        let path = self.path;
        record(
            &mut self.out,
            &[
                ("t", V::S("call")),
                ("path", V::S(path)),
                ("name", V::S(&ident.to_string())),
                ("line", V::N(line(ident.span()))),
                ("via", V::S(via)),
            ],
        );
    }

    /// A macro invocation: the call itself, then whatever its tokens parse as.
    fn mac(&mut self, mac: &syn::Macro, item_position: bool) {
        let Some(seg) = mac.path.segments.last() else { return };
        self.call(&seg.ident, "macro");
        if seg.ident == "macro_rules" {
            return;
        }
        let tokens = mac.tokens.clone();
        if tokens.is_empty() {
            return;
        }
        let as_items = || syn::parse2::<syn::File>(tokens.clone()).ok();
        let as_stmts = || syn::Block::parse_within.parse2(tokens.clone()).ok();
        let as_exprs = || {
            Punctuated::<Expr, Token![,]>::parse_terminated
                .parse2(tokens.clone())
                .ok()
        };
        if item_position && let Some(file) = as_items() {
            file.items.iter().for_each(|i| self.visit_item(i));
        } else if let Some(exprs) = as_exprs() {
            exprs.iter().for_each(|e| self.visit_expr(e));
        } else if let Some(stmts) = as_stmts() {
            stmts.iter().for_each(|s| self.visit_stmt(s));
        } else if let Some(file) = as_items() {
            file.items.iter().for_each(|i| self.visit_item(i));
        } else {
            self.lexical_calls(tokens);
            let path = self.path;
            record(
                &mut self.out,
                &[
                    ("t", V::S("opaque")),
                    ("path", V::S(path)),
                    ("name", V::S(&seg.ident.to_string())),
                    ("line", V::N(line(seg.ident.span()))),
                ],
            );
        }
    }

    /// Calls in a body no grammar parses (`json!({"k": f()})`, `warn!(x = ?g(), "...")`): an
    /// identifier followed by `(...)` is a call, by `!` and a group a macro call. `fn name(` is a
    /// definition, not a call.
    fn lexical_calls(&mut self, tokens: TokenStream) {
        let toks: Vec<TokenTree> = tokens.into_iter().collect();
        for (i, t) in toks.iter().enumerate() {
            match t {
                TokenTree::Group(g) => self.lexical_calls(g.stream()),
                TokenTree::Ident(id) => {
                    let after_fn = i > 0 && matches!(&toks[i - 1], TokenTree::Ident(p) if p == "fn");
                    match (toks.get(i + 1), toks.get(i + 2)) {
                        (Some(TokenTree::Group(g)), _) if g.delimiter() == Delimiter::Parenthesis && !after_fn => {
                            self.call(id, "call")
                        }
                        (Some(TokenTree::Punct(p)), Some(TokenTree::Group(_))) if p.as_char() == '!' => {
                            self.call(id, "macro")
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn fn_kind(&self) -> &'static str {
        if self.in_assoc > 0 { "method" } else { "fn" }
    }
}

impl<'ast> Visit<'ast> for Extractor<'_> {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        // A free fn nested in a method body is still a fn.
        let saved = std::mem::take(&mut self.in_assoc);
        self.def(i, &i.sig.ident.to_string(), i.sig.ident.span(), "fn");
        visit::visit_item_fn(self, i);
        self.in_assoc = saved;
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.def(i, &i.sig.ident.to_string(), i.sig.ident.span(), self.fn_kind());
        let saved = std::mem::take(&mut self.in_assoc);
        visit::visit_impl_item_fn(self, i);
        self.in_assoc = saved;
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        let kind = if i.default.is_some() { "method" } else { "method_decl" };
        self.def(i, &i.sig.ident.to_string(), i.sig.ident.span(), kind);
        let saved = std::mem::take(&mut self.in_assoc);
        visit::visit_trait_item_fn(self, i);
        self.in_assoc = saved;
    }
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "struct");
        visit::visit_item_struct(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "enum");
        visit::visit_item_enum(self, i);
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "union");
        visit::visit_item_union(self, i);
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "trait");
        self.in_assoc += 1;
        visit::visit_item_trait(self, i);
        self.in_assoc -= 1;
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "type");
        visit::visit_item_type(self, i);
    }
    fn visit_impl_item_type(&mut self, i: &'ast syn::ImplItemType) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "type");
        visit::visit_impl_item_type(self, i);
    }
    fn visit_trait_item_type(&mut self, i: &'ast syn::TraitItemType) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "assoc_type_decl");
        visit::visit_trait_item_type(self, i);
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        if i.ident != "_" {
            self.def(i, &i.ident.to_string(), i.ident.span(), "const");
        }
        visit::visit_item_const(self, i);
    }
    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "const");
        visit::visit_impl_item_const(self, i);
    }
    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        let kind = if i.default.is_some() {
            "const"
        } else {
            "assoc_const_decl"
        };
        self.def(i, &i.ident.to_string(), i.ident.span(), kind);
        visit::visit_trait_item_const(self, i);
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "static");
        visit::visit_item_static(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "mod");
        let saved = std::mem::take(&mut self.in_assoc);
        visit::visit_item_mod(self, i);
        self.in_assoc = saved;
    }
    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        if let Some(ident) = &i.ident {
            self.def(i, &ident.to_string(), ident.span(), "macro");
            return;
        }
        self.mac(&i.mac, true);
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let self_ty = type_name(&i.self_ty);
        let span = i.impl_token.span;
        self.def(i, &self_ty, span, "impl");
        if let Some((_, trait_path, _)) = &i.trait_
            && let Some(seg) = trait_path.segments.last()
        {
            let first = without_attrs(i).first().map_or(line(span), |t| line(t.span()));
            let path = self.path;
            record(
                &mut self.out,
                &[
                    ("t", V::S("impl")),
                    ("path", V::S(path)),
                    ("trait", V::S(&seg.ident.to_string())),
                    ("self_ty", V::S(&self_ty)),
                    ("line", V::N(first)),
                ],
            );
        }
        self.in_assoc += 1;
        visit::visit_item_impl(self, i);
        self.in_assoc -= 1;
    }
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        let first = without_attrs(i)
            .first()
            .map_or(line(i.use_token.span), |t| line(t.span()));
        let mut leaves = Vec::new();
        use_paths(&i.tree, String::new(), &mut leaves);
        let path = self.path;
        for module in leaves {
            record(
                &mut self.out,
                &[
                    ("t", V::S("use")),
                    ("path", V::S(path)),
                    ("module", V::S(&module)),
                    ("line", V::N(first)),
                ],
            );
        }
    }
    fn visit_foreign_item_fn(&mut self, i: &'ast syn::ForeignItemFn) {
        self.def(i, &i.sig.ident.to_string(), i.sig.ident.span(), "foreign_fn");
        visit::visit_foreign_item_fn(self, i);
    }
    fn visit_foreign_item_static(&mut self, i: &'ast syn::ForeignItemStatic) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "foreign_static");
        visit::visit_foreign_item_static(self, i);
    }
    fn visit_foreign_item_type(&mut self, i: &'ast syn::ForeignItemType) {
        self.def(i, &i.ident.to_string(), i.ident.span(), "foreign_type");
        visit::visit_foreign_item_type(self, i);
    }
    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*i.func
            && let Some(seg) = p.path.segments.last()
        {
            self.call(&seg.ident, "call");
        }
        visit::visit_expr_call(self, i);
    }
    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        self.call(&i.method, "method");
        visit::visit_expr_method_call(self, i);
    }
    fn visit_expr_macro(&mut self, i: &'ast syn::ExprMacro) {
        self.mac(&i.mac, false);
    }
    fn visit_stmt_macro(&mut self, i: &'ast syn::StmtMacro) {
        self.mac(&i.mac, true);
    }
    fn visit_impl_item_macro(&mut self, i: &'ast syn::ImplItemMacro) {
        self.mac(&i.mac, true);
    }
    fn visit_trait_item_macro(&mut self, i: &'ast syn::TraitItemMacro) {
        self.mac(&i.mac, true);
    }
    // A macro in type position carries no call or definition an agent looks up.
    fn visit_type_macro(&mut self, _: &'ast syn::TypeMacro) {}
    // Attributes are metadata (`#[cfg(...)]`, `#[serde(...)]`), not call sites.
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
}

/// Extract every record from one file's source.
pub fn extract(path: &str, src: &str) -> String {
    let mut ex = Extractor {
        path,
        out: String::new(),
        in_assoc: 0,
    };
    match syn::parse_file(src) {
        Ok(file) => ex.visit_file(&file),
        Err(e) => {
            let msg = e.to_string();
            record(
                &mut ex.out,
                &[("t", V::S("error")), ("path", V::S(path)), ("msg", V::S(&msg))],
            );
        }
    }
    ex.out
}

fn main() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let mut header = String::new();
    loop {
        header.clear();
        if input.read_line(&mut header)? == 0 {
            break;
        }
        let name = header.trim_end_matches('\n').to_string();
        header.clear();
        input.read_line(&mut header)?;
        let len: usize = header
            .trim()
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("length for {name}: {e}")))?;
        let mut buf = vec![0u8; len];
        input.read_exact(&mut buf)?;
        let src = String::from_utf8_lossy(&buf);
        out.write_all(extract(&name, &src).as_bytes())?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::extract;

    fn lines(out: &str, t: &str) -> Vec<String> {
        out.lines()
            .filter(|l| l.contains(&format!("\"t\":\"{t}\"")))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn reports_definitions_at_their_first_non_attribute_line() {
        let out = extract("a.rs", "/// doc\n#[inline]\npub fn spin() {}\n");
        let defs = lines(&out, "def");
        assert_eq!(defs.len(), 1);
        assert!(defs[0].contains("\"name\":\"spin\""), "{out}");
        assert!(defs[0].contains("\"line\":3,\"name_line\":3,\"end\":3"), "{out}");
    }

    #[test]
    fn reports_calls_inside_macro_bodies_and_items_inside_item_macros() {
        let src = "cfg_rt! {\n    pub fn spawn() {}\n}\nfn t() {\n    assert_eq!(make(), 1);\n    x\n        .collect::<Vec<_>>();\n}\n";
        let out = extract("a.rs", src);
        let defs = lines(&out, "def");
        assert!(
            defs.iter()
                .any(|d| d.contains("\"name\":\"spawn\"") && d.contains("\"line\":2")),
            "{out}"
        );
        let calls = lines(&out, "call");
        assert!(
            calls
                .iter()
                .any(|c| c.contains("\"name\":\"make\"") && c.contains("\"line\":5")),
            "{out}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.contains("\"name\":\"collect\"") && c.contains("\"line\":7")),
            "{out}"
        );
        assert!(calls.iter().any(|c| c.contains("\"name\":\"assert_eq\"")), "{out}");
    }

    #[test]
    fn reports_trait_impls_by_last_segment_and_methods_by_context() {
        let src = "impl<T> fmt::Display for Wrap<T> {\n    fn fmt(&self) {}\n}\ntrait Run { fn run(&self); }\n";
        let out = extract("a.rs", src);
        let impls = lines(&out, "impl");
        assert_eq!(impls.len(), 1, "{out}");
        assert!(
            impls[0].contains("\"trait\":\"Display\",\"self_ty\":\"Wrap\",\"line\":1"),
            "{out}"
        );
        assert!(out.contains("\"name\":\"fmt\",\"kind\":\"method\""), "{out}");
        assert!(out.contains("\"name\":\"run\",\"kind\":\"method_decl\""), "{out}");
    }

    #[test]
    fn finds_calls_lexically_in_bodies_no_grammar_parses() {
        let out = extract("a.rs", "fn t() {\n    json!({\"k\": make(1)});\n}\n");
        let calls = lines(&out, "call");
        assert!(
            calls
                .iter()
                .any(|c| c.contains("\"name\":\"make\"") && c.contains("\"line\":2")),
            "{out}"
        );
        assert_eq!(lines(&out, "opaque").len(), 1, "{out}");
    }

    #[test]
    fn flattens_use_trees_into_full_paths() {
        let out = extract("a.rs", "\nuse crate::rt::{task::{self, Id}, spawn as go, io::*};\n");
        let uses = lines(&out, "use");
        let modules: Vec<&str> = [
            "crate::rt::task::self",
            "crate::rt::task::Id",
            "crate::rt::spawn",
            "crate::rt::io::*",
        ]
        .into_iter()
        .filter(|m| {
            uses.iter()
                .any(|u| u.contains(&format!("\"module\":\"{m}\",\"line\":2")))
        })
        .collect();
        assert_eq!(modules.len(), 4, "{out}");
    }

    #[test]
    fn reports_a_parse_error_instead_of_silently_returning_nothing() {
        let out = extract("bad.rs", "fn (");
        assert_eq!(lines(&out, "error").len(), 1, "{out}");
    }
}
