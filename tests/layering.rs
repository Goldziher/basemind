//! Import-graph test for the library crate layering of ADR-0023.
//!
//! Every module of the library is assigned to the crate it moves to (`basemind-core`,
//! `basemind-retrieval`, `basemind-engine` or the root `basemind`). Dependencies may only point
//! down that list: a module may name a module of its own layer or a lower one, never a higher one.
//! A layer may also not name the external crates ADR-0023 forbids it (core: tokio, rmcp, clap,
//! jsonschema, xberg, lancedb; retrieval and engine: rmcp, clap). Test-only code may use them,
//! since they stay available as dev-dependencies; it may not reach a higher layer, because that
//! module will not exist in the lower crate's test build.
//!
//! The module tree is walked from `src/lib.rs` following `mod` declarations (including
//! `#[path]`), so a `*_tests.rs` file belongs to the module that declares it. Every `#[cfg]` arm is
//! read: a feature-gated reference is still a reference. Paths are collected from `use` trees
//! (grouped, renamed, glob, `pub use`), from paths in types and expressions, from macro bodies and
//! from attribute arguments. A path counts when it starts with `crate`, `self`, `super`, a child
//! module name, or a forbidden external crate. A path that resolves to the crate root itself (a
//! re-export such as `crate::Config`) names no module and is not an edge.
//!
//! `KNOWN_VIOLATIONS` lists the edges not yet cut; the test passes only on an exact match, so a new
//! edge fails it and a cut edge must be removed from the list.

use std::collections::BTreeSet;
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
/// `retrieval_wiring` is the first engine module: it binds the scan to the retrieval stack.
const LAYERS: &[(&str, Layer)] = &[
    ("alloc_tag", Core),
    ("backpressure", Core),
    ("boot_identity", Core),
    ("chunk", Core),
    ("chunk_drive", Core),
    ("cli", Root),
    ("comms", Root),
    ("config", Core),
    ("config::overrides", Root),
    ("daemon_lock", Core),
    ("disk_space", Core),
    ("embeddings", Retrieval),
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
    ("git_history::remote", Root),
    ("hashing", Core),
    ("index", Core),
    ("intel", Core),
    ("lance", Retrieval),
    ("lang", Core),
    ("lang_rules", Core),
    ("mcp", Root),
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
    ("scanner_policy", Core),
    ("scanner_vectors", Retrieval),
    ("search", Core),
    ("store", Core),
    ("store_base", Core),
    ("store_base_retention", Core),
    ("store_blob", Core),
    ("store_blob_codec", Core),
    ("store_cache_admin", Core),
    ("store_doc_tombstone", Core),
    ("store_gc", Core),
    ("store_gc_budget", Core),
    ("store_gc_live", Core),
    ("store_gc_namespaces", Core),
    ("store_gc_protect", Core),
    ("store_gc_workspace", Core),
    ("store_hash_refs", Core),
    ("store_layout", Core),
    ("store_lock", Core),
    ("store_migration", Core),
    ("store_pending", Core),
    ("store_seed", Core),
    ("sysres", Core),
    ("testing", Root),
    ("textmatch", Core),
    ("version", Core),
    ("watcher", Root),
];

/// External crates each layer must not name outside test code (ADR-0023, decision table and rule
/// 6). `tokio_util` is tokio's own companion crate and falls under the tokio ban.
const FORBIDDEN_CRATES: &[(Layer, &[&str])] = &[
    (
        Core,
        &["tokio", "tokio_util", "rmcp", "clap", "jsonschema", "xberg", "lancedb"],
    ),
    (Retrieval, &["rmcp", "clap"]),
    (Engine, &["rmcp", "clap"]),
];

/// Edges not yet cut, as `"<file> -> <target>"`, with ` [test]` when the reference sits in test
/// code. `<target>` is the module named (or `extern <crate>`). Remove an entry when its edge is cut.
const KNOWN_VIOLATIONS: &[&str] = &[
    "src/config/mod.rs -> config::overrides",
    "src/config/validate.rs -> extern jsonschema",
    "src/scanner_filter.rs -> retrieval_wiring",
    "src/store.rs -> lance",
    "src/store_blob.rs -> extract::doc",
    "src/store_blob_tests.rs -> extract::doc [test]",
];

/// One module of the library: its path, the file holding it, whether it is test-only, the names of
/// its child modules and its own items (child modules excluded).
struct Module {
    path: Vec<String>,
    file: PathBuf,
    test: bool,
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
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        let text = list.tokens.to_string().replace(' ', "");
        text == "test" || text.starts_with("all(test,") || text.contains(",test,") || text.ends_with(",test)")
    })
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
        test: bool,
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
            let child_test = test || is_cfg_test(&item_mod.attrs);
            let mut child_path = path.clone();
            child_path.push(name.clone());
            if let Some((_, content)) = item_mod.content {
                let dir = child_dir.join(&name);
                self.module(child_path, file, child_test, content, &dir, &dir);
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
            self.files.insert(child_file.clone());
            let parent = child_file.parent().expect("module file has a parent").to_path_buf();
            let grand_dir = if mod_rs { parent.clone() } else { parent.join(&name) };
            self.module(child_path, &child_file, child_test, parsed.items, &grand_dir, &parent);
        }
        self.out.push(Module {
            path,
            file: file.to_path_buf(),
            test,
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

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {}

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let mut prefix = Vec::new();
        if item.leading_colon.is_none() {
            self.use_tree(&mut prefix, &item.tree);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.leading_colon.is_none() {
            self.push(path.segments.iter().map(|s| s.ident.to_string()).collect());
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
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
    let src = root.join("src");
    let mut modules = Vec::new();
    let mut files = BTreeSet::new();
    let lib = src.join("lib.rs");
    files.insert(lib.clone());
    let parsed = parse_file(&lib);
    Walk {
        out: &mut modules,
        files: &mut files,
    }
    .module(Vec::new(), &lib, false, parsed.items, &src, &src);
    (modules, files)
}

/// Every layering violation of the library under `root`, as sorted `"<file> -> <target>"` lines.
fn violations(root: &Path, modules: &[Module]) -> BTreeSet<String> {
    let known: BTreeSet<Vec<String>> = modules.iter().map(|m| m.path.clone()).collect();
    let mut found = BTreeSet::new();
    for module in modules.iter().filter(|m| !m.path.is_empty()) {
        let Some(from) = layer_of(&module.path) else { continue };
        let mut collector = Collector {
            test: module.test,
            refs: Vec::new(),
        };
        for item in &module.items {
            collector.visit_item(item);
        }
        let file = module
            .file
            .strip_prefix(root)
            .unwrap_or(&module.file)
            .display()
            .to_string()
            .replace('\\', "/");
        for raw in collector.refs {
            let tag = if raw.test { " [test]" } else { "" };
            let first = raw.segments[0].as_str();
            let absolute: Vec<String> = match first {
                "crate" => raw.segments[1..].to_vec(),
                "self" => module.path.iter().chain(&raw.segments[1..]).cloned().collect(),
                "super" => {
                    let ups = raw.segments.iter().take_while(|s| *s == "super").count();
                    let keep = module.path.len().saturating_sub(ups);
                    module.path[..keep]
                        .iter()
                        .chain(&raw.segments[ups..])
                        .cloned()
                        .collect()
                }
                _ if module.children.contains(first) => module.path.iter().chain(&raw.segments).cloned().collect(),
                _ => {
                    if !raw.test && forbidden_for(from).contains(&first) {
                        found.insert(format!("{file} -> extern {first}"));
                    }
                    continue;
                }
            };
            let target = (0..=absolute.len())
                .rev()
                .map(|n| &absolute[..n])
                .find(|p| known.contains(*p));
            let Some(target) = target.filter(|t| !t.is_empty()) else {
                continue;
            };
            let Some(to) = layer_of(target) else { continue };
            if to > from {
                found.insert(format!("{file} -> {}{tag}", target.join("::")));
            }
        }
    }
    found
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

    let main = root.join("src/main.rs");
    let parsed = parse_file(&main);
    let src = root.join("src");
    let mut bin_modules = Vec::new();
    reached.insert(main.clone());
    Walk {
        out: &mut bin_modules,
        files: &mut reached,
    }
    .module(Vec::new(), &main, false, parsed.items, &src, &src);

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

/// The analyzer must see each reference shape it claims to handle; a fixture crate proves it.
#[test]
fn the_analyzer_flags_each_reference_shape() {
    let dir = std::env::temp_dir().join(format!("basemind-layering-fixture-{}", std::process::id()));
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("config")).expect("create fixture dirs");
    let write = |rel: &str, body: &str| std::fs::write(src.join(rel), body).expect("write fixture file");
    write(
        "lib.rs",
        "pub mod config;\npub mod mcp;\npub mod store;\npub mod cli;\npub mod lance;\n",
    );
    write("mcp.rs", "pub mod params { pub struct V; }\n");
    write("cli.rs", "pub fn run() {}\n");
    write("lance.rs", "pub struct L;\n");
    write("config/mod.rs", "pub mod overrides;\npub use overrides::O;\n");
    write("config/overrides.rs", "pub struct O;\n");
    write(
        "store.rs",
        "use crate::{mcp::params::{self, V}, config};\n\
         pub fn f() { crate::cli::run(); tokio::spawn(async {}); let _ = format!(\"{:?}\", super::lance::L); }\n\
         #[derive(Default)] #[serde(with = \"crate::mcp\")] pub struct S;\n\
         #[cfg(test)] mod tests { #[tokio::test] async fn t() { let _ = crate::cli::run(); } }\n",
    );
    let (modules, _) = load_modules(&dir);
    let found = violations(&dir, &modules);
    std::fs::remove_dir_all(&dir).expect("remove fixture");
    let expected: BTreeSet<String> = [
        "src/config/mod.rs -> config::overrides",
        "src/store.rs -> cli",
        "src/store.rs -> cli [test]",
        "src/store.rs -> extern tokio",
        "src/store.rs -> lance",
        "src/store.rs -> mcp",
        "src/store.rs -> mcp::params",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(found, expected);
}
