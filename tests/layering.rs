//! Import-graph test for the library crate layering of ADR-0023.
//!
//! Every module of the library is assigned to the crate it moves to (`basemind-core`,
//! `basemind-retrieval`, `basemind-engine` or the root `basemind`). Dependencies may only point
//! down that list: a module may name a module of its own layer or a lower one, never a higher one.
//! A layer may also not name the external crates ADR-0023 forbids it (core: tokio, rmcp, clap,
//! jsonschema, xberg, lancedb; retrieval: rmcp, clap; engine: rmcp, clap and, by ADR-0039, tokio).
//! Test-only code may use them, since they stay available as dev-dependencies; it may not reach a
//! higher layer, because that module will not exist in the lower crate's test build.
//!
//! The module tree is walked from `src/lib.rs` following `mod` declarations (including
//! `#[path]`), so a `*_tests.rs` file belongs to the module that declares it. Every `#[cfg]` arm is
//! read: a feature-gated reference is still a reference. Paths are collected from `use` trees
//! (grouped, renamed, glob, `pub use`), from paths in types and expressions, from macro bodies and
//! from attribute arguments. A path counts when it starts with `crate`, `self`, `super`, a child
//! module name, or a forbidden external crate. A `cfg_attr(test, ..)` argument and a `#[cfg(test)]`
//! trait or impl method are test code.
//!
//! A path is followed through re-exports: when the segment after its longest module prefix is a
//! `pub use` of that module (named or glob), resolution continues at the re-exported path, and every
//! module passed on the way is an edge. A `#[macro_export]` macro lives at the crate root, so
//! `crate::name!` and a bare `name!` are edges to the module that defines it. The crate root
//! (`src/lib.rs`) counts as core: its `pub use` items are edges like any other module's.
//!
//! `KNOWN_VIOLATIONS` lists the edges not yet cut; the test passes only on an exact match, so a new
//! edge fails it and a cut edge must be removed from the list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Layer {
    Core,
    Retrieval,
    Engine,
    Root,
}

use Layer::{Core, Engine, Retrieval, Root};

/// Module path prefix -> future crate. The longest matching prefix wins, so a module listed here
/// carries its submodules (and the `*_tests.rs` files it declares) unless a deeper entry overrides.
/// `retrieval_wiring` binds the scan to the retrieval stack; `engine` holds the synchronous
/// `Workspace` (ADR-0039).
const LAYERS: &[(&str, Layer)] = &[
    ("alloc_tag", Core),
    ("allocator", Core),
    ("backpressure", Core),
    ("boot_identity", Core),
    ("chunk", Core),
    ("chunk_drive", Core),
    ("cli", Root),
    ("comms", Root),
    ("config", Core),
    ("config::overrides", Root),
    ("daemon_lock", Core),
    ("data_root", Core),
    ("deps_docs", Core),
    ("disk_space", Core),
    ("embeddings", Retrieval),
    ("engine", Engine),
    ("eval", Root),
    ("extract", Core),
    ("extract::doc", Retrieval),
    ("extract::doc_cost", Retrieval),
    ("extract::doc_fallback", Retrieval),
    ("extract::doc_guard", Retrieval),
    ("extract::doc_tiers", Retrieval),
    ("git", Core),
    ("git_cache", Core),
    ("git_history", Core),
    ("hashing", Core),
    ("index", Core),
    ("intel", Core),
    ("lance", Retrieval),
    ("lang", Core),
    ("lang_grammars", Core),
    ("lang_rules", Core),
    ("lead", Root),
    ("mcp", Root),
    ("once_map", Core),
    ("path", Core),
    ("process_start", Core),
    ("query", Core),
    ("registry", Core),
    ("render", Root),
    ("retrieval_wiring", Engine),
    ("scan_evidence", Core),
    ("scan_evidence_hosts", Core),
    ("scan_phase_peaks", Core),
    ("scan_retrieval", Core),
    ("scanner", Core),
    ("scanner_candidates", Core),
    ("scanner_code", Core),
    ("scanner_docs", Retrieval),
    ("scanner_docs_lane", Retrieval),
    ("scanner_drive", Core),
    ("scanner_file", Core),
    ("scanner_filter", Core),
    ("scanner_flush", Core),
    ("scanner_index_batch", Core),
    ("scanner_integrity", Core),
    ("scanner_lanes", Core),
    ("scanner_lines", Core),
    ("scanner_named", Core),
    ("scanner_policy", Core),
    ("scanner_vectors", Retrieval),
    ("search", Core),
    ("sessions", Root),
    ("store", Core),
    ("store_blob", Core),
    ("store_blob_codec", Core),
    ("store_cache_admin", Core),
    ("store_doc_tombstone", Core),
    ("store_gc", Core),
    ("store_gc_budget", Core),
    ("store_gc_groups", Core),
    ("store_gc_live", Core),
    ("store_gc_namespaces", Core),
    ("store_gc_protect", Core),
    ("store_gc_workspace", Core),
    ("store_group_live", Core),
    ("store_group_sweep", Core),
    ("store_hash_refs", Core),
    ("store_layout", Core),
    ("store_lock", Core),
    ("store_pending", Core),
    ("sysres", Core),
    ("testing", Root),
    ("textmatch", Core),
    ("version", Core),
    ("watch_health", Core),
    ("watcher", Root),
    ("wire", Core),
];

/// External crates each layer must not name outside test code (ADR-0023, decision table and rule
/// 6). `tokio_util` is tokio's own companion crate and falls under the tokio ban.
const FORBIDDEN_CRATES: &[(Layer, &[&str])] = &[
    (
        Core,
        &["tokio", "tokio_util", "rmcp", "clap", "jsonschema", "xberg", "lancedb"],
    ),
    (Retrieval, &["rmcp", "clap"]),
    (Engine, &["rmcp", "clap", "tokio", "tokio_util"]),
];

/// Edges not yet cut, as `"<file> -> <target>"`, with ` [test]` when the reference sits in test
/// code. `<target>` is the module named (or `extern <crate>`). Remove an entry when its edge is cut.
/// The `src/engine` edges are the interim ones of ADR-0039, each removed by the engine-unification
/// step named beside it.
const KNOWN_VIOLATIONS: &[&str] = &["src/config/validate.rs -> extern jsonschema", "src/store.rs -> lance"];

/// One module of the library: its path, the file holding it, whether it is test-only or a test hook
/// (under the `test-support` feature, it or a module enclosing it), the names of its child modules
/// and its own items (child modules excluded).
struct Module {
    path: Vec<String>,
    file: PathBuf,
    test: bool,
    hook: bool,
    children: BTreeSet<String>,
    items: Vec<syn::Item>,
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("test") {
            return true;
        }
        if attr.path().segments.last().is_some_and(|seg| seg.ident == "test") {
            return true;
        }
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| cfg_requires_test(&meta))
    })
}

/// Whether a `cfg` predicate holds only in test builds: `test`, an `all(..)` with such an argument,
/// or an `any(..)` whose every argument is one. `not(..)` and features never are.
fn cfg_requires_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => {
            let Ok(args) =
                list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                args.iter().any(cfg_requires_test)
            } else if list.path.is_ident("any") {
                !args.is_empty() && args.iter().all(cfg_requires_test)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| {
        if !attr.path().is_ident("path") {
            return None;
        }
        let syn::Meta::NameValue(nv) = &attr.meta else {
            return None;
        };
        match &nv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s), ..
            }) => Some(s.value()),
            _ => None,
        }
    })
}

fn parse_file(file: &Path) -> syn::File {
    let source = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    syn::parse_file(&source).unwrap_or_else(|e| panic!("parse {}: {e}", file.display()))
}

/// Walk one module and its descendants. `child_dir` is where `mod foo;` without `#[path]` looks;
/// `path_dir` is what a `#[path]` attribute is relative to.
struct Walk<'a> {
    out: &'a mut Vec<Module>,
    files: &'a mut BTreeSet<PathBuf>,
}

impl Walk<'_> {
    fn module(
        &mut self,
        path: Vec<String>,
        file: &Path,
        gate: Gate,
        items: Vec<syn::Item>,
        child_dir: &Path,
        path_dir: &Path,
    ) {
        let mut children = BTreeSet::new();
        let mut own = Vec::new();
        for item in items {
            let syn::Item::Mod(item_mod) = item else {
                own.push(item);
                continue;
            };
            let name = item_mod.ident.to_string();
            children.insert(name.clone());
            let child_gate = gate.within(&item_mod.attrs);
            let mut child_path = path.clone();
            child_path.push(name.clone());
            if let Some((_, content)) = item_mod.content {
                let dir = child_dir.join(&name);
                self.module(child_path, file, child_gate, content, &dir, &dir);
                continue;
            }
            let (child_file, mod_rs) = match path_attr(&item_mod.attrs) {
                Some(rel) => (path_dir.join(rel), true),
                None => {
                    let flat = child_dir.join(format!("{name}.rs"));
                    if flat.exists() {
                        (flat, false)
                    } else {
                        (child_dir.join(&name).join("mod.rs"), true)
                    }
                }
            };
            let parsed = parse_file(&child_file);
            let child_gate = child_gate.within(&parsed.attrs);
            self.files.insert(child_file.clone());
            let parent = child_file.parent().expect("module file has a parent").to_path_buf();
            let grand_dir = if mod_rs { parent.clone() } else { parent.join(&name) };
            self.module(child_path, &child_file, child_gate, parsed.items, &grand_dir, &parent);
        }
        self.out.push(Module {
            path,
            file: file.to_path_buf(),
            test: gate == Gate::Test,
            hook: gate == Gate::TestSupport,
            children,
            items: own,
        });
    }
}

fn collect_rs_files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display())) {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.insert(path);
        }
    }
}

/// A raw path reference found in a module, before resolution.
struct RawRef {
    segments: Vec<String>,
    test: bool,
}

struct Collector {
    test: bool,
    refs: Vec<RawRef>,
}

impl Collector {
    fn push(&mut self, segments: Vec<String>) {
        if segments.len() >= 2 || segments.first().is_some_and(|s| s == "crate" || s == "super") {
            self.refs.push(RawRef {
                segments,
                test: self.test,
            });
        }
    }

    fn use_tree(&mut self, prefix: &mut Vec<String>, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.use_tree(prefix, &p.tree);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                let mut full = prefix.clone();
                if n.ident != "self" {
                    full.push(n.ident.to_string());
                }
                self.push_use(full);
            }
            syn::UseTree::Rename(r) => {
                let mut full = prefix.clone();
                if r.ident != "self" {
                    full.push(r.ident.to_string());
                }
                self.push_use(full);
            }
            syn::UseTree::Glob(_) => self.push_use(prefix.clone()),
            syn::UseTree::Group(g) => {
                for item in &g.items {
                    self.use_tree(prefix, item);
                }
            }
        }
    }

    /// A `use` of a bare crate or module (`use tokio;`, `use crate::mcp::{self}`) is still a
    /// reference, so single-segment use paths are kept.
    fn push_use(&mut self, segments: Vec<String>) {
        if !segments.is_empty() {
            self.refs.push(RawRef {
                segments,
                test: self.test,
            });
        }
    }

    /// Scan a token stream (macro body, attribute arguments) for `a::b::c` ident chains.
    fn tokens(&mut self, stream: TokenStream) {
        let mut flat: Vec<Option<String>> = Vec::new();
        flatten(stream, &mut flat);
        let mut i = 0;
        while i < flat.len() {
            let Some(first) = flat[i].as_deref().filter(|t| is_ident(t)) else {
                i += 1;
                continue;
            };
            let preceded = i >= 2 && flat[i - 1].as_deref() == Some(":") && flat[i - 2].as_deref() == Some(":");
            let mut segments = vec![first.to_string()];
            let mut j = i + 1;
            while flat.get(j).and_then(|t| t.as_deref()) == Some(":")
                && flat.get(j + 1).and_then(|t| t.as_deref()) == Some(":")
            {
                match flat.get(j + 2).and_then(|t| t.as_deref()).filter(|t| is_ident(t)) {
                    Some(next) => segments.push(next.to_string()),
                    None => break,
                }
                j += 3;
            }
            if !preceded {
                let bare_macro = segments.len() == 1
                    && flat.get(j).and_then(|t| t.as_deref()) == Some("!")
                    && flat.get(j + 1).and_then(|t| t.as_deref()) != Some("=");
                if bare_macro {
                    self.push(vec!["crate".to_string(), segments[0].clone()]);
                }
                self.push(segments);
            }
            i = j.max(i + 1);
        }
    }
}

fn is_ident(token: &str) -> bool {
    token.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
}

/// Flatten a token stream to identifiers and single punctuation characters; string literals that
/// look like paths (`with = "crate::a::b"`) are re-tokenized, everything else is a break (`None`).
fn flatten(stream: TokenStream, out: &mut Vec<Option<String>>) {
    for tree in stream {
        match tree {
            TokenTree::Group(group) => {
                out.push(None);
                flatten(group.stream(), out);
                out.push(None);
            }
            TokenTree::Ident(ident) => out.push(Some(ident.to_string())),
            TokenTree::Punct(punct) => out.push(Some(punct.as_char().to_string())),
            TokenTree::Literal(lit) => {
                let text = lit.to_string();
                let inner = text.trim_matches('"');
                if text.starts_with('"')
                    && inner.contains("::")
                    && let Ok(stream) = inner.parse::<TokenStream>()
                {
                    out.push(None);
                    flatten(stream, out);
                }
                out.push(None);
            }
        }
    }
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let saved = self.test;
        self.test |= is_cfg_test(item_attrs(item));
        syn::visit::visit_item(self, item);
        self.test = saved;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let saved = self.test;
        self.test |= is_cfg_test(&item.attrs);
        syn::visit::visit_impl_item_fn(self, item);
        self.test = saved;
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        let saved = self.test;
        self.test |= is_cfg_test(&item.attrs);
        syn::visit::visit_trait_item_fn(self, item);
        self.test = saved;
    }

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {}

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let mut prefix = Vec::new();
        self.use_tree(&mut prefix, &item.tree);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.push(path.segments.iter().map(|s| s.ident.to_string()).collect());
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // A bare `name!` may be a `#[macro_export]` macro, which lives at the crate root.
        if let Some(name) = mac.path.get_ident() {
            self.push(vec!["crate".to_string(), name.to_string()]);
        }
        self.tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        // Doc comments are prose: a path named in one is not a dependency.
        if attr.path().is_ident("doc") {
            return;
        }
        if let Some(rest) = cfg_attr_test_args(attr) {
            let saved = self.test;
            self.test = true;
            self.tokens(rest);
            self.test = saved;
            return;
        }
        match &attr.meta {
            syn::Meta::List(list) => self.tokens(list.tokens.clone()),
            syn::Meta::NameValue(nv) => {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s), ..
                }) = &nv.value
                    && s.value().contains("::")
                    && let Ok(stream) = s.value().parse::<TokenStream>()
                {
                    self.tokens(stream);
                }
            }
            syn::Meta::Path(_) => {}
        }
        syn::visit::visit_attribute(self, attr);
    }
}

/// The attributes a `cfg_attr` applies only in test builds, `None` for any other attribute.
fn cfg_attr_test_args(attr: &syn::Attribute) -> Option<TokenStream> {
    if !attr.path().is_ident("cfg_attr") {
        return None;
    }
    let syn::Meta::List(list) = &attr.meta else {
        return None;
    };
    let (predicate, rest) = list
        .parse_args_with(|input: syn::parse::ParseStream| {
            let predicate: syn::Meta = input.parse()?;
            input.parse::<syn::Token![,]>()?;
            let rest: TokenStream = input.parse()?;
            Ok((predicate, rest))
        })
        .ok()?;
    cfg_requires_test(&predicate).then_some(rest)
}

fn layer_of(module: &[String]) -> Option<Layer> {
    let joined = module.join("::");
    LAYERS
        .iter()
        .filter(|(prefix, _)| joined == *prefix || joined.starts_with(&format!("{prefix}::")))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, layer)| *layer)
}

fn forbidden_for(layer: Layer) -> &'static [&'static str] {
    FORBIDDEN_CRATES
        .iter()
        .find(|(l, _)| *l == layer)
        .map_or(&[], |(_, crates)| crates)
}

/// The library's module tree, walked from `<root>/src/lib.rs`, plus every file it reached.
fn load_modules(root: &Path) -> (Vec<Module>, BTreeSet<PathBuf>) {
    let mut modules = Vec::new();
    let mut files = BTreeSet::new();
    walk_crate(&root.join("src").join("lib.rs"), &mut modules, &mut files);
    (modules, files)
}

/// Walk the module tree of the crate whose root file is `crate_root` into `modules`, recording every
/// file it reaches in `files`.
fn walk_crate(crate_root: &Path, modules: &mut Vec<Module>, files: &mut BTreeSet<PathBuf>) {
    let dir = crate_root.parent().expect("crate root has a parent");
    files.insert(crate_root.to_path_buf());
    let parsed = parse_file(crate_root);
    Walk { out: modules, files }.module(Vec::new(), crate_root, Gate::Open, parsed.items, dir, dir);
}

/// `segments` as a path from the crate root, or `None` when it starts with neither `crate`, `self`,
/// `super` nor a child module name (an external crate, a prelude name or a local).
fn absolutize(module: &Module, segments: &[String]) -> Option<Vec<String>> {
    let first = segments.first()?.as_str();
    Some(match first {
        "crate" => segments[1..].to_vec(),
        "self" => module.path.iter().chain(&segments[1..]).cloned().collect(),
        "super" => {
            let ups = segments.iter().take_while(|s| *s == "super").count();
            let keep = module.path.len().saturating_sub(ups);
            module.path[..keep].iter().chain(&segments[ups..]).cloned().collect()
        }
        _ if module.children.contains(first) => module.path.iter().chain(segments).cloned().collect(),
        _ => return None,
    })
}

/// What a module makes reachable under its own path: its `pub use` items (named and glob) and the
/// names it defines; plus the crate's `#[macro_export]` macros with the module defining each.
#[derive(Default)]
struct Exports {
    named: BTreeMap<(Vec<String>, String), Vec<String>>,
    globs: BTreeMap<Vec<String>, Vec<Vec<String>>>,
    defined: BTreeSet<(Vec<String>, String)>,
    macros: BTreeMap<String, Vec<String>>,
}

fn item_name(item: &syn::Item) -> Option<String> {
    let ident = match item {
        syn::Item::Const(i) => &i.ident,
        syn::Item::Enum(i) => &i.ident,
        syn::Item::Fn(i) => &i.sig.ident,
        syn::Item::Macro(i) => i.ident.as_ref()?,
        syn::Item::Static(i) => &i.ident,
        syn::Item::Struct(i) => &i.ident,
        syn::Item::Trait(i) => &i.ident,
        syn::Item::TraitAlias(i) => &i.ident,
        syn::Item::Type(i) => &i.ident,
        syn::Item::Union(i) => &i.ident,
        _ => return None,
    };
    Some(ident.to_string())
}

/// The `(name, path)` pairs a `use` tree brings in; a glob has no name.
fn use_pairs(prefix: &mut Vec<String>, tree: &syn::UseTree, out: &mut Vec<(Option<String>, Vec<String>)>) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            use_pairs(prefix, &p.tree, out);
            prefix.pop();
        }
        syn::UseTree::Name(n) if n.ident == "self" => {
            if let Some(last) = prefix.last() {
                out.push((Some(last.clone()), prefix.clone()));
            }
        }
        syn::UseTree::Name(n) => {
            let mut full = prefix.clone();
            full.push(n.ident.to_string());
            out.push((Some(n.ident.to_string()), full));
        }
        syn::UseTree::Rename(r) => {
            let mut full = prefix.clone();
            if r.ident != "self" {
                full.push(r.ident.to_string());
            }
            out.push((Some(r.rename.to_string()), full));
        }
        syn::UseTree::Glob(_) => out.push((None, prefix.clone())),
        syn::UseTree::Group(g) => {
            for item in &g.items {
                use_pairs(prefix, item, out);
            }
        }
    }
}

fn exports(modules: &[Module]) -> Exports {
    let mut out = Exports::default();
    for module in modules {
        for item in &module.items {
            if let Some(name) = item_name(item) {
                out.defined.insert((module.path.clone(), name));
            }
            match item {
                syn::Item::Macro(mac) if mac.attrs.iter().any(|a| a.path().is_ident("macro_export")) => {
                    if let Some(ident) = &mac.ident {
                        out.macros.insert(ident.to_string(), module.path.clone());
                    }
                }
                syn::Item::Use(item_use) if !matches!(item_use.vis, syn::Visibility::Inherited) => {
                    let mut pairs = Vec::new();
                    use_pairs(&mut Vec::new(), &item_use.tree, &mut pairs);
                    for (name, target) in pairs {
                        let Some(target) = absolutize(module, &target) else {
                            continue;
                        };
                        match name {
                            Some(name) => {
                                out.named.insert((module.path.clone(), name), target);
                            }
                            None => out.globs.entry(module.path.clone()).or_default().push(target),
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Every module an absolute path passes through: its longest module prefix and, while the next
/// segment is a re-export there (or a `#[macro_export]` macro at the root), each module along the
/// re-export chain. Bounded, so a re-export cycle cannot loop.
fn modules_passed(known: &BTreeSet<Vec<String>>, exports: &Exports, mut absolute: Vec<String>) -> Vec<Vec<String>> {
    let mut passed = Vec::new();
    for _ in 0..16 {
        let depth = (0..=absolute.len())
            .rev()
            .find(|&n| known.contains(&absolute[..n]))
            .unwrap_or(0);
        let module = absolute[..depth].to_vec();
        if !module.is_empty() {
            passed.push(module.clone());
        }
        let Some(name) = absolute.get(depth).cloned() else {
            break;
        };
        let key = (module.clone(), name.clone());
        let provides = |target: &Vec<String>| {
            let key = (target.clone(), name.clone());
            exports.defined.contains(&key) || exports.named.contains_key(&key) || {
                let mut child = target.clone();
                child.push(name.clone());
                known.contains(&child)
            }
        };
        absolute = if let Some(target) = exports.named.get(&key) {
            target.iter().chain(&absolute[depth + 1..]).cloned().collect()
        } else if module.is_empty()
            && let Some(defining) = exports.macros.get(&name)
        {
            defining.clone()
        } else if !exports.defined.contains(&key)
            && let Some(target) = exports
                .globs
                .get(&module)
                .and_then(|globs| globs.iter().find(|g| provides(g)))
        {
            target.iter().chain(&absolute[depth..]).cloned().collect()
        } else {
            break;
        };
    }
    passed
}

/// Every layering violation of the library under `root`, as sorted `"<file> -> <target>"` lines.
fn violations(root: &Path, modules: &[Module]) -> BTreeSet<String> {
    let known: BTreeSet<Vec<String>> = modules.iter().map(|m| m.path.clone()).collect();
    let exports = exports(modules);
    let mut found = BTreeSet::new();
    for module in modules {
        let from = if module.path.is_empty() {
            Core
        } else {
            let Some(from) = layer_of(&module.path) else { continue };
            from
        };
        let mut collector = Collector {
            test: module.test,
            refs: Vec::new(),
        };
        for item in &module.items {
            collector.visit_item(item);
        }
        let file = repo_relative(root, &module.file);
        for raw in collector.refs {
            let tag = if raw.test { " [test]" } else { "" };
            let Some(absolute) = absolutize(module, &raw.segments) else {
                let first = raw.segments[0].as_str();
                if !raw.test && forbidden_for(from).contains(&first) {
                    found.insert(format!("{file} -> extern {first}"));
                }
                continue;
            };
            for target in modules_passed(&known, &exports, absolute) {
                let Some(to) = layer_of(&target) else { continue };
                if to > from {
                    found.insert(format!("{file} -> {}{tag}", target.join("::")));
                }
            }
        }
    }
    found
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `file` relative to `root`, written with `/` whatever the platform, so a finding reads the same
/// on Windows as the allowlists and expectations written here.
fn repo_relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string()
        .replace('\\', "/")
}

#[test]
fn repo_relative_writes_windows_separators_as_slashes() {
    assert_eq!(
        repo_relative(Path::new("/repo"), Path::new(r"/repo/src\config\documents.rs")),
        "src/config/documents.rs"
    );
    assert_eq!(
        repo_relative(Path::new("/repo"), Path::new("/repo/src/data_root.rs")),
        "src/data_root.rs"
    );
}

#[test]
fn every_module_is_assigned_to_a_crate_and_every_file_is_reached() {
    let root = manifest_dir();
    let (modules, mut reached) = load_modules(&root);
    assert!(
        modules.len() > 300,
        "walked only {} modules from src/lib.rs",
        modules.len()
    );

    let unassigned: Vec<String> = modules
        .iter()
        .filter(|m| !m.path.is_empty() && layer_of(&m.path).is_none())
        .map(|m| m.path.join("::"))
        .collect();
    assert!(
        unassigned.is_empty(),
        "modules with no crate in LAYERS (ADR-0023):\n  {}",
        unassigned.join("\n  ")
    );

    let paths: BTreeSet<String> = modules.iter().map(|m| m.path.join("::")).collect();
    let stale: Vec<&str> = LAYERS.iter().map(|(p, _)| *p).filter(|p| !paths.contains(*p)).collect();
    assert!(stale.is_empty(), "LAYERS entries naming no module: {stale:?}");

    let src = root.join("src");
    walk_crate(&src.join("main.rs"), &mut Vec::new(), &mut reached);

    let mut all = BTreeSet::new();
    collect_rs_files(&src, &mut all);
    let orphans: Vec<String> = all.difference(&reached).map(|p| p.display().to_string()).collect();
    assert!(
        orphans.is_empty(),
        "src files reached from neither lib.rs nor main.rs:\n  {}",
        orphans.join("\n  ")
    );
}

#[test]
fn no_module_reaches_a_higher_layer() {
    let root = manifest_dir();
    let (modules, _) = load_modules(&root);
    let found = violations(&root, &modules);
    let known: BTreeSet<String> = KNOWN_VIOLATIONS.iter().map(|s| (*s).to_string()).collect();

    let new: Vec<&String> = found.difference(&known).collect();
    let cut: Vec<&String> = known.difference(&found).collect();
    let listing = found
        .iter()
        .map(|v| format!("    \"{v}\","))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        new.is_empty() && cut.is_empty(),
        "layering violations differ from KNOWN_VIOLATIONS.\nnew (cut them, ADR-0023): {new:#?}\n\
         no longer found (remove from KNOWN_VIOLATIONS): {cut:#?}\nall {} found:\n{listing}",
        found.len(),
    );
}

/// The analyzer must see each reference shape it claims to handle; a fixture crate proves it. Each
/// core file below holds one shape, so each expected line proves that shape is seen.
#[test]
fn the_analyzer_flags_each_reference_shape() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("config")).expect("create fixture dirs");
    std::fs::create_dir_all(src.join("git")).expect("create fixture dirs");
    let write = |rel: &str, body: &str| std::fs::write(src.join(rel), body).expect("write fixture file");
    write(
        "lib.rs",
        "pub mod config;\npub mod mcp;\npub mod store;\npub mod cli;\npub mod lance;\npub mod git;\n\
         pub mod index;\npub mod query;\npub mod textmatch;\npub mod path;\npub mod hashing;\npub mod version;\n\
         pub use mcp::params::V as Laundered;\npub use lance::*;\n",
    );
    write(
        "git.rs",
        "#[path = \"git/walk_impl.rs\"]\nmod walk;\nmod nested;\n/// Mentions crate::cli in prose only.\npub fn g() {}\n",
    );
    write(
        "git/walk_impl.rs",
        "use crate::cli as c;\npub fn w() { ::tokio::spawn(async {}); }\n",
    );
    write(
        "git/nested.rs",
        "use super::super::lance::*;\n#[cfg(any(feature = \"x\", test))]\nfn either() { tokio::spawn(async {}); }\n",
    );
    write(
        "mcp.rs",
        "pub mod params { pub struct V; }\n#[macro_export]\nmacro_rules! mcp_macro { () => { $crate::cli::run() }; }\n",
    );
    write("cli.rs", "pub fn run() {}\n");
    write(
        "lance.rs",
        "pub struct L;\n#[macro_export]\nmacro_rules! lance_macro { () => {}; }\n",
    );
    write("config/mod.rs", "pub mod overrides;\npub use overrides::O;\n");
    write("config/overrides.rs", "pub struct O;\n");
    write(
        "store.rs",
        "use crate::{mcp::params::{self, V}, config};\n\
         pub fn f() { crate::cli::run(); tokio::spawn(async {}); let _ = format!(\"{:?}\", super::lance::L); }\n\
         #[derive(Default)] #[serde(with = \"crate::mcp\")] pub struct S;\n\
         #[cfg(test)] mod tests { #[tokio::test] async fn t() { let _ = crate::cli::run(); } }\n",
    );
    // A bare invocation of a `#[macro_export]` macro, nested in another macro's arguments.
    write("index.rs", "pub fn i() { let _ = vec![mcp_macro!()]; }\n");
    // A bare top-level invocation of a `#[macro_export]` macro.
    write("textmatch.rs", "pub fn t() { mcp_macro!(); }\n");
    // A `crate::`-qualified invocation of a `#[macro_export]` macro.
    write("query.rs", "pub fn q() { crate::lance_macro!(); }\n");
    // A call through a lower-layer module's re-export, and a test-only trait method.
    write(
        "path.rs",
        "pub fn p() { let _ = crate::config::O; }\npub trait T { #[cfg(test)] fn t() { crate::cli::run(); } }\n",
    );
    // A call through a renamed re-export in the crate root, and a test-only `cfg_attr`.
    write(
        "hashing.rs",
        "pub fn h() { let _ = crate::Laundered; }\n#[cfg_attr(test, serde(with = \"crate::cli\"))] pub struct H;\n",
    );
    // A call through a glob re-export in the crate root.
    write("version.rs", "pub fn v() -> crate::L { crate::L }\n");
    let (modules, _) = load_modules(dir.path());
    let found = violations(dir.path(), &modules);
    let expected: BTreeSet<String> = [
        "src/config/mod.rs -> config::overrides",
        "src/git/nested.rs -> extern tokio",
        "src/git/nested.rs -> lance",
        "src/git/walk_impl.rs -> cli",
        "src/git/walk_impl.rs -> extern tokio",
        "src/hashing.rs -> cli [test]",
        "src/hashing.rs -> mcp::params",
        "src/index.rs -> mcp",
        "src/lib.rs -> lance",
        "src/lib.rs -> mcp::params",
        "src/path.rs -> cli [test]",
        "src/path.rs -> config::overrides",
        "src/query.rs -> lance",
        "src/store.rs -> cli",
        "src/store.rs -> cli [test]",
        "src/store.rs -> extern tokio",
        "src/store.rs -> lance",
        "src/store.rs -> mcp",
        "src/store.rs -> mcp::params",
        "src/textmatch.rs -> mcp",
        "src/version.rs -> lance",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(found, expected);
}

/// Names an engine-layer item may not use: the MCP adapter's types (ADR-0039). The engine answers
/// with its own result types and `EngineError`; the adapter turns them into tool results.
const ADAPTER_TYPES: &[&str] = &["McpError", "CallToolResult", "ErrorData", "Parameters"];

/// What makes an engine item depend on its adapter: an `async fn`, an adapter type, or a
/// non-private function answering with a `serde_json::Value` instead of a typed result.
struct EngineApi {
    test: bool,
    found: BTreeSet<String>,
}

impl EngineApi {
    fn function(&mut self, vis: Option<&syn::Visibility>, sig: &syn::Signature) {
        if self.test {
            return;
        }
        let name = sig.ident.to_string();
        if sig.asyncness.is_some() {
            self.found.insert(format!("async fn {name}"));
        }
        let exported = vis.is_some_and(|vis| !matches!(vis, syn::Visibility::Inherited));
        if let syn::ReturnType::Type(_, ty) = &sig.output
            && exported
            && returns_json_value(ty)
        {
            self.found.insert(format!("fn {name} -> serde_json::Value"));
        }
    }

    fn tokens(&mut self, stream: TokenStream) {
        for tree in stream {
            match tree {
                TokenTree::Ident(ident) => self.ident(&ident.to_string()),
                TokenTree::Group(group) => self.tokens(group.stream()),
                _ => {}
            }
        }
    }

    fn ident(&mut self, ident: &str) {
        if !self.test && ADAPTER_TYPES.contains(&ident) {
            self.found.insert(format!("names {ident}"));
        }
    }
}

/// `serde_json::Value`, or a bare `Value` (the only `Value` an engine module imports).
fn returns_json_value(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else { return false };
    let segments: Vec<String> = path.path.segments.iter().map(|s| s.ident.to_string()).collect();
    matches!(segments.as_slice(), [only] if only == "Value")
        || matches!(segments.as_slice(), [krate, value] if krate == "serde_json" && value == "Value")
}

impl<'ast> Visit<'ast> for EngineApi {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let saved = self.test;
        self.test |= is_cfg_test(item_attrs(item));
        syn::visit::visit_item(self, item);
        self.test = saved;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.function(Some(&item.vis), &item.sig);
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let saved = self.test;
        self.test |= is_cfg_test(&item.attrs);
        self.function(Some(&item.vis), &item.sig);
        syn::visit::visit_impl_item_fn(self, item);
        self.test = saved;
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        let saved = self.test;
        self.test |= is_cfg_test(&item.attrs);
        self.function(None, &item.sig);
        syn::visit::visit_trait_item_fn(self, item);
        self.test = saved;
    }

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {}

    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        self.ident(&ident.to_string());
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }
}

/// Every adapter dependency in `items`, as `EngineApi` reports them.
fn engine_api_findings(items: &[syn::Item], test: bool) -> BTreeSet<String> {
    let mut api = EngineApi {
        test,
        found: BTreeSet::new(),
    };
    for item in items {
        api.visit_item(item);
    }
    api.found
}

/// The engine is a synchronous library (ADR-0039): no non-test engine-layer item is async, names
/// an MCP adapter type, or answers with an untyped JSON value.
#[test]
fn engine_api_is_sync_and_adapter_free() {
    let root = manifest_dir();
    let (modules, _) = load_modules(&root);
    let mut checked = 0;
    let mut offences = Vec::new();
    for module in modules.iter().filter(|m| layer_of(&m.path) == Some(Engine)) {
        checked += 1;
        let file = module
            .file
            .strip_prefix(&root)
            .unwrap_or(&module.file)
            .display()
            .to_string();
        for finding in engine_api_findings(&module.items, module.test) {
            offences.push(format!("{file}: {finding}"));
        }
    }
    assert!(checked >= 15, "checked only {checked} engine-layer modules");
    assert!(
        offences.is_empty(),
        "engine-layer code depends on its adapter:\n  {}",
        offences.join("\n  ")
    );
}

/// The check sees each shape it claims to, and leaves test code and private helpers alone.
#[test]
fn the_engine_api_check_flags_each_shape() {
    let file = syn::parse_file(
        "pub(crate) async fn warm() {}\n\
         pub(crate) fn answer() -> Result<CallToolResult, McpError> { todo!() }\n\
         pub fn raw() -> serde_json::Value { serde_json::Value::Null }\n\
         fn private() -> Value { Value::Null }\n\
         pub(crate) fn typed() -> Outline { format!(\"{}\", Parameters) }\n\
         #[cfg(test)] mod tests { async fn t() -> McpError { todo!() } }\n\
         pub trait T { async fn go(&self); }\n\
         #[cfg(test)] fn helper() -> ErrorData { todo!() }\n",
    )
    .expect("parse");
    let expected: BTreeSet<String> = [
        "async fn go",
        "async fn warm",
        "fn raw -> serde_json::Value",
        "names CallToolResult",
        "names McpError",
        "names Parameters",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(engine_api_findings(&file.items, false), expected);
}

/// `(rmcp_files, helper_lines)`: the non-test files under `src/mcp` that name rmcp, `McpError` or
/// `CallToolResult`, and the lines of the non-test `src/mcp/helpers*.rs` files. Each engine step
/// moves tool bodies out of the adapter, so neither may rise; lower them here when they fall.
const ADAPTER_RATCHET: (usize, usize) = (30, 92);

/// A test file under `src/mcp`: every test module there lives in a `*_tests.rs` file.
fn is_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("_tests.rs"))
}

#[test]
fn the_adapter_only_shrinks() {
    let root = manifest_dir();
    let mcp = root.join("src/mcp");
    let mut files = BTreeSet::new();
    collect_rs_files(&mcp, &mut files);
    let mut rmcp_files = 0;
    let mut helper_lines = 0;
    let mut scanned = 0;
    for file in files.iter().filter(|file| !is_test_file(file)) {
        scanned += 1;
        let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        if ["rmcp", "McpError", "CallToolResult"]
            .iter()
            .any(|name| text.contains(name))
        {
            rmcp_files += 1;
        }
        let helper = file.parent() == Some(mcp.as_path())
            && file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("helpers"));
        if helper {
            helper_lines += text.lines().count();
        }
    }
    assert!(scanned > 30, "scanned only {scanned} files under src/mcp");
    let found = (rmcp_files, helper_lines);
    assert!(
        rmcp_files <= ADAPTER_RATCHET.0 && helper_lines <= ADAPTER_RATCHET.1,
        "src/mcp grew to (rmcp_files, helper_lines) = {found:?}, over the ratchet {ADAPTER_RATCHET:?}: \
         put the new body in src/engine, not in the adapter."
    );
    if found != ADAPTER_RATCHET {
        println!("src/mcp shrank to {found:?}: lower ADAPTER_RATCHET from {ADAPTER_RATCHET:?}.");
    }
}

/// The in-process forwarding seams the engine unification deleted (ADR-0039): a process that does
/// not own the index forwards its whole call, a daemon-hosted session shares the pool writer's
/// index and git-history handles, and a session holding no index refuses rather than reading an
/// in-RAM projection. None of these names may come back anywhere under `src/`.
const RETIRED_SYMBOLS: &[&str] = &[
    "HostBackend",
    "HistoryHost",
    "HistoryForwarder",
    "IndexRoute",
    "daemon_writer",
    "scan_calls_in_ram",
    "scan_impls_in_ram",
];

/// Whether `text` names `symbol` as a whole identifier.
fn names_symbol(text: &str, symbol: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(symbol).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + symbol.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

#[test]
fn retired_symbols_stay_gone() {
    assert!(names_symbol("use crate::mcp::HostBackend;", "HostBackend"));
    assert!(names_symbol("a `daemon_writer` serve", "daemon_writer"));
    assert!(!names_symbol("IndexRouteTable", "IndexRoute"));

    let root = manifest_dir();
    let mut files = BTreeSet::new();
    collect_rs_files(&root.join("src"), &mut files);
    assert!(files.len() > 300, "scanned only {} files under src", files.len());
    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for symbol in RETIRED_SYMBOLS {
            if names_symbol(&text, symbol) {
                found.push(format!("{} names {symbol}", file.display()));
            }
        }
    }
    assert!(found.is_empty(), "retired symbols are back:\n{}", found.join("\n"));
}

/// Environment reads allowed below the root layer, as `"<file> -> <path>"`, each with its count.
/// `Env { env }` is a secret the operator names in `basemind.toml` by the variable that holds it
/// (ADR-0050): the indirection is the configuration. Everything else reaches the library through
/// the engine configuration, which `src/cli/env_config.rs` builds from the environment.
/// The two `ProjectDirs` lookups resolve the documented platform defaults (the data home of a process
/// that installed none, the user-level agents policy) from `HOME` / `XDG_*`, which is their
/// definition; an embedder overrides both in code.
const ALLOWED_ENV_READS: &[(&str, usize)] = &[
    ("src/config/agents_policy.rs -> directories::ProjectDirs::from", 1),
    ("src/config/documents.rs -> std::env::var", 1),
    ("src/data_root.rs -> directories::ProjectDirs::from", 1),
];

const ENV_READS: &[&str] = &["var", "var_os", "vars", "vars_os"];

/// The `directories` types whose constructors read `HOME` and the `XDG_*` variables.
const ENV_DIRS: &[&str] = &["ProjectDirs", "BaseDirs", "UserDirs"];

/// Whether a `cfg` predicate holds only in test builds or with the `test-support` feature, which
/// only the test suite enables: the hooks a test drives through the environment.
fn cfg_is_test_hook(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::NameValue(nv) => {
            nv.path.is_ident("feature")
                && matches!(&nv.value, syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) if s.value() == "test-support")
        }
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => {
            let Ok(args) =
                list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                args.iter().any(cfg_is_test_hook)
            } else if list.path.is_ident("any") {
                !args.is_empty() && args.iter().all(cfg_is_test_hook)
            } else {
                false
            }
        }
    }
}

fn is_test_hook(attrs: &[syn::Attribute]) -> bool {
    is_cfg_test(attrs)
        || attrs.iter().any(|attr| {
            attr.path().is_ident("cfg") && attr.parse_args::<syn::Meta>().is_ok_and(|meta| cfg_is_test_hook(&meta))
        })
}

/// The prefix of the environment variables only tests set.
const TEST_VAR_PREFIX: &str = "BASEMIND_TEST_";

/// The gate a `BASEMIND_TEST_` literal sits under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// None: every build compiles it.
    Open,
    /// The `test-support` feature (or a test build): a `test-support` binary carries it.
    TestSupport,
    /// A test build only.
    Test,
}

impl Gate {
    /// The gate of code under `attrs` that sits inside code under this one.
    fn within(self, attrs: &[syn::Attribute]) -> Self {
        if self == Self::Test || is_cfg_test(attrs) {
            Self::Test
        } else if self == Self::TestSupport || is_test_hook(attrs) {
            Self::TestSupport
        } else {
            Self::Open
        }
    }
}

/// Walks a module's items, tracking whether the code sits in test code or a test hook. Outside
/// them it collects `env::var`-family paths, through `std::env` under any alias, and the
/// `directories` constructors that read the environment implicitly; everywhere it collects the
/// `BASEMIND_TEST_` string literals with the gate each sits under.
#[derive(Default)]
struct HookScan {
    hook: bool,
    test: bool,
    found: Vec<String>,
    /// Names `std::env` is imported under (`use std::env as e;`).
    aliases: BTreeSet<String>,
    test_vars: Vec<(String, Gate)>,
}

impl HookScan {
    fn note_literal(&mut self, value: String) {
        if !value.starts_with(TEST_VAR_PREFIX) {
            return;
        }
        let gate = if self.test {
            Gate::Test
        } else if self.hook {
            Gate::TestSupport
        } else {
            Gate::Open
        };
        self.test_vars.push((value, gate));
    }

    fn check(&mut self, segments: &[String]) {
        if self.hook {
            return;
        }
        let n = segments.len();
        let reads_env = n >= 2
            && (segments[n - 2] == "env" || self.aliases.contains(&segments[n - 2]))
            && ENV_READS.contains(&segments[n - 1].as_str());
        let resolves_dirs = segments[..n.saturating_sub(1)]
            .iter()
            .any(|segment| ENV_DIRS.contains(&segment.as_str()));
        if reads_env || resolves_dirs {
            self.found.push(segments.join("::"));
        }
    }

    /// Record the names `tree` imports `std::env` (or `core::env`) under.
    fn note_aliases(&mut self, tree: &syn::UseTree) {
        let mut pairs = Vec::new();
        use_pairs(&mut Vec::new(), tree, &mut pairs);
        for (name, segments) in pairs {
            if let Some(name) = name
                && segments.len() == 2
                && matches!(segments[0].as_str(), "std" | "core")
                && segments[1] == "env"
            {
                self.aliases.insert(name);
            }
        }
    }

    fn scoped(&mut self, attrs: &[syn::Attribute], visit: impl FnOnce(&mut Self)) {
        let saved = (self.hook, self.test);
        self.hook |= is_test_hook(attrs);
        self.test |= is_cfg_test(attrs);
        visit(self);
        (self.hook, self.test) = saved;
    }
}

/// The string literals in a macro body, which `syn` leaves unparsed.
fn macro_string_literals(stream: TokenStream, out: &mut Vec<String>) {
    for tree in stream {
        match tree {
            TokenTree::Group(group) => macro_string_literals(group.stream(), out),
            TokenTree::Literal(lit) => {
                if let Ok(text) = syn::parse_str::<syn::LitStr>(&lit.to_string()) {
                    out.push(text.value());
                }
            }
            TokenTree::Ident(_) | TokenTree::Punct(_) => {}
        }
    }
}

impl<'ast> Visit<'ast> for HookScan {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        self.scoped(item_attrs(item), |v| syn::visit::visit_item(v, item));
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.scoped(&item.attrs, |v| syn::visit::visit_impl_item_fn(v, item));
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.scoped(&item.attrs, |v| syn::visit::visit_trait_item_fn(v, item));
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        self.scoped(&local.attrs, |v| syn::visit::visit_local(v, local));
    }

    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        let attrs: &[syn::Attribute] = match expr {
            syn::Expr::If(e) => &e.attrs,
            syn::Expr::Block(e) => &e.attrs,
            syn::Expr::Call(e) => &e.attrs,
            syn::Expr::MethodCall(e) => &e.attrs,
            syn::Expr::Match(e) => &e.attrs,
            syn::Expr::Let(e) => &e.attrs,
            syn::Expr::Unsafe(e) => &e.attrs,
            syn::Expr::Macro(e) => &e.attrs,
            _ => &[],
        };
        self.scoped(attrs, |v| syn::visit::visit_expr(v, expr));
    }

    fn visit_stmt_macro(&mut self, mac: &'ast syn::StmtMacro) {
        self.scoped(&mac.attrs, |v| syn::visit::visit_stmt_macro(v, mac));
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.scoped(&arm.attrs, |v| syn::visit::visit_arm(v, arm));
    }

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {}

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if is_test_hook(&item.attrs) {
            return;
        }
        self.note_aliases(&item.tree);
        let mut pairs = Vec::new();
        use_pairs(&mut Vec::new(), &item.tree, &mut pairs);
        for (_, segments) in pairs {
            self.check(&segments);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        self.check(&segments);
        syn::visit::visit_path(self, path);
    }

    fn visit_lit_str(&mut self, lit: &'ast syn::LitStr) {
        self.note_literal(lit.value());
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let mut literals = Vec::new();
        macro_string_literals(mac.tokens.clone(), &mut literals);
        for literal in literals {
            self.note_literal(literal);
        }
        let mut flat = Vec::new();
        flatten(mac.tokens.clone(), &mut flat);
        for window in flat.windows(4) {
            if let [Some(env), Some(c1), Some(c2), Some(read)] = window
                && env == "env"
                && c1 == ":"
                && c2 == ":"
                && ENV_READS.contains(&read.as_str())
            {
                self.check(&["env".to_string(), read.clone()]);
            }
        }
        syn::visit::visit_macro(self, mac);
    }
}

/// Scan a module's `items`; `hook` when the module itself sits under the `test-support` feature.
fn scan_items(items: &[syn::Item], hook: bool) -> HookScan {
    let mut scan = HookScan {
        hook,
        ..HookScan::default()
    };
    // A module-level alias may be declared below its first use.
    for item in items {
        if let syn::Item::Use(item) = item
            && !is_test_hook(&item.attrs)
        {
            scan.note_aliases(&item.tree);
        }
    }
    for item in items {
        scan.visit_item(item);
    }
    scan
}

fn env_reads(items: &[syn::Item], hook: bool) -> Vec<String> {
    scan_items(items, hook).found
}

/// The `BASEMIND_TEST_` literals of `items`, in source order, each with the gate it sits under;
/// `hook` when the module itself sits under the `test-support` feature.
fn test_var_literals(items: &[syn::Item], hook: bool) -> Vec<(String, Gate)> {
    scan_items(items, hook).test_vars
}

/// Core, retrieval and engine code read no environment variable (ADR-0050): the operator's settings
/// arrive as the engine configuration, so an embedder configures the library in code and the CLI
/// keeps every variable's meaning in one place.
#[test]
fn no_env_reads_below_root() {
    let root = manifest_dir();
    let (modules, _) = load_modules(&root);
    let mut checked = 0;
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for module in modules
        .iter()
        .filter(|m| !m.test && matches!(layer_of(&m.path), Some(Core | Retrieval | Engine)))
    {
        checked += 1;
        let file = repo_relative(&root, &module.file);
        for read in env_reads(&module.items, module.hook) {
            *found.entry(format!("{file} -> {read}")).or_default() += 1;
        }
    }
    assert!(checked >= 150, "checked only {checked} modules below the root layer");
    let allowed: BTreeMap<String, usize> = ALLOWED_ENV_READS.iter().map(|(k, n)| ((*k).to_string(), *n)).collect();
    assert_eq!(
        found, allowed,
        "environment reads below the root layer differ from ALLOWED_ENV_READS; map the variable onto \
         EngineConfig in src/cli/env_config.rs instead"
    );
}

/// The check sees each spelling of a read, and leaves test code and test hooks alone.
#[test]
fn the_env_read_check_flags_each_shape() {
    let file = syn::parse_file(
        "use std::env;\n\
         use std::env::var_os;\n\
         fn a() { std::env::var(\"A\"); }\n\
         fn b() { env::var_os(\"B\"); }\n\
         fn c() { let _ = format!(\"{:?}\", std::env::vars()); }\n\
         fn d() { #[cfg(test)] if std::env::var_os(\"D\").is_some() {} }\n\
         #[cfg(any(test, feature = \"test-support\"))] fn e() { std::env::var(\"E\"); }\n\
         #[cfg(feature = \"test-support\")] fn f() { std::env::var(\"F\"); }\n\
         #[cfg(feature = \"documents\")] fn g() { std::env::var(\"G\"); }\n\
         fn h() { std::env::temp_dir(); std::env::split_paths(\"\"); }\n\
         fn i() { e::var(\"I\"); }\n\
         use std::env as e;\n\
         use directories::BaseDirs;\n\
         fn j() { directories::ProjectDirs::from(\"\", \"\", \"x\"); BaseDirs::new(); }\n\
         fn k(dirs: &directories::BaseDirs) -> std::path::PathBuf { dirs.cache_dir().to_path_buf() }\n",
    )
    .expect("parse");
    assert_eq!(
        env_reads(&file.items, false),
        [
            "std::env::var_os",
            "std::env::var",
            "env::var_os",
            "env::vars",
            "std::env::var",
            "e::var",
            "directories::ProjectDirs::from",
            "BaseDirs::new"
        ]
    );
}

/// The crate roots whose code the `basemind` binary is built from and so may read a test variable.
const BINARY_CRATE_ROOTS: &[&str] = &["src/lib.rs", "src/main.rs", "crates/basemind-agents/src/lib.rs"];

/// The release-archive smoke, which sets every variable a `test-support` binary reads.
const RELEASE_SMOKE: &str = "scripts/smoke_release_archive.py";

/// Every `BASEMIND_TEST_` literal outside the test files sits under `cfg(test)` or the
/// `test-support` feature, which only the test suite enables, so a release build neither names nor
/// reads one. Each one a `test-support` binary reads is set by the release-archive smoke, which
/// proves the shipped binary ignores it.
#[test]
fn test_variables_are_read_only_under_a_test_gate() {
    let root = manifest_dir();
    let mut modules = Vec::new();
    let mut reached = BTreeSet::new();
    for crate_root in BINARY_CRATE_ROOTS {
        walk_crate(&root.join(crate_root), &mut modules, &mut reached);
    }
    let mut open = Vec::new();
    let mut test_support = BTreeSet::new();
    for module in modules.iter().filter(|m| !m.test) {
        for (name, gate) in test_var_literals(&module.items, module.hook) {
            match gate {
                Gate::Open => open.push(format!("{} names {name}", repo_relative(&root, &module.file))),
                Gate::TestSupport => {
                    test_support.insert(name);
                }
                Gate::Test => {}
            }
        }
    }
    assert!(
        open.is_empty(),
        "test variables named outside cfg(test) and the test-support feature:\n{}",
        open.join("\n")
    );

    let mut sources = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("crates")).expect("read crates/") {
        let src = entry.expect("read crates/ entry").path().join("src");
        if src.is_dir() {
            collect_rs_files(&src, &mut sources);
        }
    }
    let unchecked: Vec<String> = sources
        .difference(&reached)
        .filter(|file| {
            std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()))
                .contains(TEST_VAR_PREFIX)
        })
        .map(|file| repo_relative(&root, file))
        .collect();
    assert!(
        unchecked.is_empty(),
        "files naming a test variable in a crate missing from BINARY_CRATE_ROOTS:\n{}",
        unchecked.join("\n")
    );

    assert!(
        test_support.len() >= 3,
        "found only {test_support:?} under the test-support feature"
    );
    let smoke =
        std::fs::read_to_string(root.join(RELEASE_SMOKE)).unwrap_or_else(|e| panic!("read {RELEASE_SMOKE}: {e}"));
    let unsmoked: Vec<&String> = test_support.difference(&smoke_sets(&smoke)).collect();
    assert!(
        unsmoked.is_empty(),
        "test-support variables {RELEASE_SMOKE} does not set on the release binary: {unsmoked:?}"
    );
}

/// The `BASEMIND_TEST_` variables the release smoke puts in the binary's environment: the keys of
/// its `TEST_HOOKS` dict, when the sandbox applies it with `env.update(TEST_HOOKS)`, and the
/// `env["…"] = …` assignments. Only code counts: a name in a comment or a docstring sets nothing.
fn smoke_sets(smoke: &str) -> BTreeSet<String> {
    let mut hooks = BTreeSet::new();
    let mut assigned = BTreeSet::new();
    let mut applied = false;
    let mut in_docstring = false;
    let mut in_hooks = false;
    for line in smoke.lines() {
        let code = line.trim();
        if code.matches("\"\"\"").count() % 2 == 1 {
            in_docstring = !in_docstring;
            continue;
        }
        if in_docstring || code.starts_with('#') {
            continue;
        }
        if in_hooks {
            if code.starts_with('}') {
                in_hooks = false;
            } else if let Some((name, tail)) = code.strip_prefix('"').and_then(|rest| rest.split_once('"'))
                && tail.starts_with(':')
            {
                hooks.insert(name.to_string());
            }
            continue;
        }
        if code.starts_with("TEST_HOOKS = {") {
            in_hooks = true;
        } else if code == "env.update(TEST_HOOKS)" {
            applied = true;
        } else if let Some((name, tail)) = code.strip_prefix("env[\"").and_then(|rest| rest.split_once("\"]"))
            && tail.trim_start().starts_with('=')
        {
            assigned.insert(name.to_string());
        }
    }
    if applied {
        assigned.extend(hooks);
    }
    assigned.retain(|name| name.starts_with(TEST_VAR_PREFIX));
    assigned
}

/// The smoke check counts a variable the sandbox sets, never one only a comment or docstring names.
#[test]
fn the_smoke_check_counts_only_variables_the_sandbox_sets() {
    let smoke = "\"\"\"Sets ``\"BASEMIND_TEST_DOC\": 1``.\n\nAlso \"BASEMIND_TEST_DOC2\": x\n\"\"\"\n\
                 # \"BASEMIND_TEST_COMMENT\"\n\
                 TEST_HOOKS = {\n\
                 \x20   \"BASEMIND_TEST_A\": \"1\",\n\
                 \x20   # \"BASEMIND_TEST_COMMENTED\": \"1\",\n\
                 }\n\
                 def sandbox_env(root):\n\
                 \x20   env = {}\n\
                 \x20   env.update(TEST_HOOKS)\n\
                 \x20   env[\"BASEMIND_TEST_B\"] = str(root)\n\
                 \x20   env[\"BASEMIND_DATA_HOME\"] = str(root)\n\
                 \x20   return env\n";
    let names = |set: BTreeSet<String>| set.into_iter().collect::<Vec<_>>();
    assert_eq!(names(smoke_sets(smoke)), ["BASEMIND_TEST_A", "BASEMIND_TEST_B"]);
    let unapplied = smoke.replace("    env.update(TEST_HOOKS)\n", "");
    assert_eq!(names(smoke_sets(&unapplied)), ["BASEMIND_TEST_B"]);
}

/// A module under the `test-support` feature gates the literals inside it and inside its children,
/// and leaves its sibling's open.
#[test]
fn a_test_support_module_gates_its_literals_and_not_its_siblings() {
    let file = syn::parse_file(
        "#[cfg(feature = \"test-support\")] mod hooks {\n\
         \x20   const NAME: &str = \"BASEMIND_TEST_GATED\";\n\
         \x20   mod inner { const NAME: &str = \"BASEMIND_TEST_NESTED\"; }\n\
         }\n\
         mod open { const NAME: &str = \"BASEMIND_TEST_OPEN\"; }\n\
         #[cfg(test)] mod tests { const NAME: &str = \"BASEMIND_TEST_IN_TESTS\"; }\n",
    )
    .expect("parse");
    let mut modules = Vec::new();
    let mut files = BTreeSet::new();
    let here = Path::new(".");
    Walk {
        out: &mut modules,
        files: &mut files,
    }
    .module(Vec::new(), Path::new("fixture.rs"), Gate::Open, file.items, here, here);
    let mut found: Vec<(String, String, Gate)> = modules
        .iter()
        .filter(|module| !module.test)
        .flat_map(|module| {
            test_var_literals(&module.items, module.hook)
                .into_iter()
                .map(|(name, gate)| (module.path.join("::"), name, gate))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let found: Vec<(&str, &str, Gate)> = found
        .iter()
        .map(|(path, name, gate)| (path.as_str(), name.as_str(), *gate))
        .collect();
    assert_eq!(
        found,
        [
            ("hooks", "BASEMIND_TEST_GATED", Gate::TestSupport),
            ("hooks::inner", "BASEMIND_TEST_NESTED", Gate::TestSupport),
            ("open", "BASEMIND_TEST_OPEN", Gate::Open),
        ]
    );
    let tests = modules
        .iter()
        .find(|module| module.path == ["tests"])
        .expect("the tests module");
    assert!(tests.test && !tests.hook, "a cfg(test) module is test code, not a hook");
    assert!(files.is_empty(), "inline modules read no file: {files:?}");
}

/// The check sees a test variable in a constant, a call and a macro body, and tells the gates apart.
#[test]
fn the_test_variable_check_tells_the_gates_apart() {
    let file = syn::parse_file(
        "/// Reads `BASEMIND_TEST_DOC`.\n\
         const A: &str = \"BASEMIND_TEST_A\";\n\
         fn b() { std::env::var(\"BASEMIND_TEST_B\"); }\n\
         fn c() { assert!(true, \"{}\", \"BASEMIND_TEST_C\"); }\n\
         #[cfg(feature = \"test-support\")] const D: &str = \"BASEMIND_TEST_D\";\n\
         fn e() { #[cfg(feature = \"test-support\")] if std::env::var_os(\"BASEMIND_TEST_E\").is_some() {} }\n\
         #[cfg(any(test, feature = \"test-support\"))] const F: &str = \"BASEMIND_TEST_F\";\n\
         #[cfg(test)] const G: &str = \"BASEMIND_TEST_G\";\n\
         #[cfg(all(test, unix))] fn h() { std::env::var(\"BASEMIND_TEST_H\"); }\n\
         #[cfg(feature = \"documents\")] const I: &str = \"BASEMIND_TEST_I\";\n\
         #[cfg(not(feature = \"test-support\"))] const J: &str = \"BASEMIND_TEST_J\";\n\
         const K: &str = \"BASEMIND_OTHER\";\n",
    )
    .expect("parse");
    let found = test_var_literals(&file.items, false);
    let found: Vec<(&str, Gate)> = found.iter().map(|(name, gate)| (name.as_str(), *gate)).collect();
    assert_eq!(
        found,
        [
            ("BASEMIND_TEST_A", Gate::Open),
            ("BASEMIND_TEST_B", Gate::Open),
            ("BASEMIND_TEST_C", Gate::Open),
            ("BASEMIND_TEST_D", Gate::TestSupport),
            ("BASEMIND_TEST_E", Gate::TestSupport),
            ("BASEMIND_TEST_F", Gate::TestSupport),
            ("BASEMIND_TEST_G", Gate::Test),
            ("BASEMIND_TEST_H", Gate::Test),
            ("BASEMIND_TEST_I", Gate::Open),
            ("BASEMIND_TEST_J", Gate::Open),
        ]
    );
}

/// Collects the tokio runtime constructions (`runtime::Builder`, `Runtime::new`) outside test code.
struct RuntimeBuilds {
    test: bool,
    found: usize,
}

impl<'ast> Visit<'ast> for RuntimeBuilds {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let saved = self.test;
        self.test |= is_cfg_test(item_attrs(item));
        syn::visit::visit_item(self, item);
        self.test = saved;
    }

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {}

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        let builds = segments
            .windows(2)
            .any(|pair| pair == ["runtime", "Builder"] || pair == ["Runtime", "new"]);
        if builds && !self.test {
            self.found += 1;
        }
        syn::visit::visit_path(self, path);
    }
}

/// The library below the root layer builds tokio runtimes in one module, the retrieval stack's
/// (ADR-0023 rule 3, ADR-0050): document extraction's, and the one each vector store owns.
#[test]
fn runtime_builders() {
    let root = manifest_dir();
    let (modules, _) = load_modules(&root);
    let mut files = BTreeSet::new();
    for module in modules
        .iter()
        .filter(|m| !m.test && matches!(layer_of(&m.path), Some(Core | Retrieval | Engine)))
    {
        let mut builds = RuntimeBuilds { test: false, found: 0 };
        for item in &module.items {
            builds.visit_item(item);
        }
        if builds.found > 0 {
            files.insert(repo_relative(&root, &module.file));
        }
    }
    let expected: BTreeSet<String> = ["src/lance/runtime.rs".to_string()].into();
    assert_eq!(
        files, expected,
        "tokio runtimes are built outside the retrieval runtime module"
    );
}
