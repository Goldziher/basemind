//! File-level module resolution for languages whose imports name modules rather than bindings:
//! Rust (`mod`, `use`) and Go (package import paths).
//!
//! The binding stitch ([`crate::intel::xfile`]) joins an import to the export it binds and needs
//! per-file import/export facts. These resolvers need neither: they read the importer's L1 outline
//! from its blob, map each import to the indexed files it names, and write only what the import
//! graph consumes, the importer's file-edge row and its `importers_by_probe` rows. Those rows sit
//! under the importer's prefix like the binding stitch's, so the resolve pass clears them with the
//! rest of the importer's slate and the incremental re-stitch finds the importer through the
//! probes, exactly as it does for JS/TS.
//!
//! - [`crate::intel::module_rust`]: `mod` declarations and `crate::` / `self::` / `super::` paths
//!   to module files.
//! - [`crate::intel::module_go`]: import paths under the `go.mod` module to the package's files.

use std::path::Path;

use ahash::AHashMap;
use rayon::prelude::*;

use crate::extract::FileMapL1;
use crate::index::IndexDb;
use crate::intel::model::FileResolvedRefs;
use crate::intel::module_go::GoModules;
use crate::intel::module_rust::RustModules;
use crate::intel::resolver::ProbeSet;
use crate::intel::stage_budget::BudgetedWriter;
use crate::intel::xfile::FileEdgesBuilder;
use crate::path::RelPath;
use crate::store::Store;

/// Importers staged into one Fjall write batch, as in the binding stitch.
const COMMIT_BATCH: usize = 256;

/// What one import resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModuleTarget {
    /// The indexed files the import names: one Rust module file, or every file of a Go package.
    Files(Vec<String>),
    /// A module outside the repository: a dependency or the standard library.
    External,
    /// A module the repository should hold that no indexed file matches.
    Unresolved,
    /// The importer itself, as a Rust path into one of its own inline modules. Not an edge.
    SelfFile,
}

/// One import of an importer and its resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleImport {
    /// Byte offset of the import in the importer.
    pub anchor: u32,
    /// The import as written, kept for an unresolved one.
    pub specifier: String,
    pub target: ModuleTarget,
}

/// The lookups a resolver makes for one importer. Every repo-relative path it consults is recorded
/// in `probes`, so that adding, deleting or changing that path re-stitches the importer.
pub(crate) struct Probe<'a> {
    pub root: &'a Path,
    /// Whether a repo-relative path is an indexed file.
    pub indexed: &'a dyn Fn(&str) -> bool,
    pub probes: &'a mut ProbeSet,
}

impl Probe<'_> {
    /// Record `rel` as consulted.
    pub(crate) fn record(&mut self, rel: &str) {
        self.probes.insert_rel(&RelPath::from(rel));
    }

    /// Record `rel` and answer whether it is an indexed file.
    pub(crate) fn indexed(&mut self, rel: &str) -> bool {
        self.record(rel);
        (self.indexed)(rel)
    }
}

/// A per-language file-level module resolver. Holds caches of the files it reads outside the index
/// (`Cargo.toml`, `go.mod`, package directories) for one stitch; it records every consulted path
/// for every importer whether or not the cache answered.
pub(crate) enum ModuleResolver {
    Rust(RustModules),
    Go(GoModules),
}

impl ModuleResolver {
    /// Whether `language` (a TSLP pack name) has a file-level module resolver.
    pub(crate) fn supports(language: &str) -> bool {
        matches!(language, "rust" | "go")
    }

    fn for_language(language: &str) -> Option<Self> {
        match language {
            "rust" => Some(Self::Rust(RustModules::default())),
            "go" => Some(Self::Go(GoModules::default())),
            _ => None,
        }
    }

    fn resolve(&mut self, probe: &mut Probe<'_>, importer: &str, l1: &FileMapL1) -> Vec<ModuleImport> {
        match self {
            Self::Rust(resolver) => resolver.resolve(probe, importer, &l1.symbols, &l1.imports),
            Self::Go(resolver) => resolver.resolve(probe, importer, &l1.imports),
        }
    }
}

/// Append the files of one resolve-pass chunk that a module resolver covers. Only files whose facts
/// were staged are taken, so each one's previous rows are already cleared when it is stitched.
pub(crate) fn harvest_module_importers(facts: &[(String, FileResolvedRefs)], out: &mut Vec<String>) {
    out.extend(
        facts
            .iter()
            .filter(|(_, refs)| ModuleResolver::supports(&refs.language))
            .map(|(rel, _)| rel.clone()),
    );
}

/// Resolve every importer in `importers` with its language's module resolver and write its
/// file-edge row and probe rows. Runs after the resolve pass staged the importers' facts, which
/// cleared their previous rows. Resolution runs in parallel (each worker with its own resolver
/// caches); staging stays serial, as in the resolve pass. Returns whether every batch committed.
pub(crate) fn stitch_module_edges(root: &Path, store: &Store, index_db: &IndexDb, importers: &[String]) -> bool {
    if importers.is_empty() {
        return true;
    }
    let started = std::time::Instant::now();
    let mut keys: Vec<&String> = importers.iter().collect();
    keys.sort_unstable();
    keys.dedup();
    let resolved: Vec<(&String, Vec<ModuleImport>, ProbeSet)> = keys
        .par_iter()
        .map_init(AHashMap::new, |resolvers, importer| {
            resolve_importer(root, store, resolvers, importer)
        })
        .flatten()
        .collect();

    let mut batch = BudgetedWriter::new(index_db, COMMIT_BATCH, "module stitch");
    let rows = resolved.len();
    for (importer, imports, probes) in resolved {
        let importer_rel = RelPath::from(importer.as_str());
        if !probes.is_empty() {
            batch
                .writer()
                .upsert_import_probes(&importer_rel, &[], &probes.hashes());
            batch.item_staged();
        }
        batch
            .writer()
            .upsert_file_edges(&importer_rel, &file_edges(importer, imports).finish());
        batch.item_staged();
    }
    let committed = batch.finish();
    tracing::debug!(
        importers = keys.len(),
        rows,
        elapsed_ms = started.elapsed().as_millis(),
        "module stitch: staged file-edge rows"
    );
    committed
}

/// One importer's resolved imports and probes, or `None` when it has no resolver, no readable
/// outline or no import. `resolvers` is the calling worker's per-language cache.
fn resolve_importer<'a>(
    root: &Path,
    store: &Store,
    resolvers: &mut AHashMap<String, Option<ModuleResolver>>,
    importer: &&'a String,
) -> Option<(&'a String, Vec<ModuleImport>, ProbeSet)> {
    let importer: &'a String = importer;
    let entry = store.lookup(importer.as_str())?;
    let resolver = resolvers
        .entry(entry.language.clone())
        .or_insert_with_key(|lang| ModuleResolver::for_language(lang))
        .as_mut()?;
    let l1 = match store.read_l1_by_hex(&entry.hash_hex) {
        Ok(l1) => l1?,
        Err(error) => {
            tracing::debug!(importer = %importer, %error, "module stitch: outline unreadable — skipping");
            return None;
        }
    };
    let indexed = |rel: &str| store.lookup(rel).is_some();
    let mut probes = ProbeSet::default();
    let imports = resolver.resolve(
        &mut Probe {
            root,
            indexed: &indexed,
            probes: &mut probes,
        },
        importer,
        &l1,
    );
    (!imports.is_empty()).then_some((importer, imports, probes))
}

/// Fold one importer's resolved imports into its file edges.
fn file_edges(importer: &str, imports: Vec<ModuleImport>) -> FileEdgesBuilder {
    let mut edges = FileEdgesBuilder::default();
    for import in imports {
        match import.target {
            ModuleTarget::Files(files) => {
                for file in files.iter().filter(|file| file.as_str() != importer) {
                    edges.hit_at(RelPath::from(file.as_str()), import.anchor, false);
                }
            }
            ModuleTarget::External => edges.external(),
            ModuleTarget::Unresolved => edges.unresolved(&import.specifier),
            ModuleTarget::SelfFile => {}
        }
    }
    edges
}

/// `dir` joined with `rel`, where an empty `dir` is the repository root.
pub(crate) fn join(dir: &str, rel: &str) -> String {
    match (dir.is_empty(), rel.is_empty()) {
        (true, _) => rel.to_string(),
        (false, true) => dir.to_string(),
        (false, false) => format!("{dir}/{rel}"),
    }
}

/// The directory of a repo-relative file path (`""` at the root).
pub(crate) fn parent_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// `rel` with the directory `dir` stripped, or `None` when `rel` is not under it.
pub(crate) fn strip_dir<'a>(rel: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        return Some(rel);
    }
    rel.strip_prefix(dir)?.strip_prefix('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(anchor: u32, specifier: &str, target: ModuleTarget) -> ModuleImport {
        ModuleImport {
            anchor,
            specifier: specifier.to_string(),
            target,
        }
    }

    #[test]
    fn file_edges_count_each_outcome_and_drop_self_targets() {
        let edges = file_edges(
            "pkg/a.go",
            vec![
                import(
                    9,
                    "example.com/m/pkg",
                    ModuleTarget::Files(vec!["pkg/a.go".to_string(), "pkg/b.go".to_string()]),
                ),
                import(4, "fmt", ModuleTarget::External),
                import(20, "example.com/m/gone", ModuleTarget::Unresolved),
                import(30, "self::inner", ModuleTarget::SelfFile),
                import(
                    2,
                    "example.com/m/pkg",
                    ModuleTarget::Files(vec!["pkg/b.go".to_string()]),
                ),
            ],
        )
        .finish();
        let targets: Vec<(&str, u32)> = edges
            .edges
            .iter()
            .map(|edge| (edge.target.as_str().unwrap(), edge.local_start))
            .collect();
        assert_eq!(targets, [("pkg/b.go", 2)]);
        assert_eq!((edges.external, edges.unresolved), (1, 1));
        assert_eq!(edges.unresolved_specifiers, ["example.com/m/gone"]);
    }

    #[test]
    fn path_helpers_treat_the_empty_dir_as_the_root() {
        assert_eq!(join("", "src"), "src");
        assert_eq!(join("a", ""), "a");
        assert_eq!(join("a", "b.rs"), "a/b.rs");
        assert_eq!(parent_dir("a/b/c.rs"), "a/b");
        assert_eq!(parent_dir("c.rs"), "");
        assert_eq!(strip_dir("a/b/c.rs", "a"), Some("b/c.rs"));
        assert_eq!(strip_dir("ab/c.rs", "a"), None);
        assert_eq!(strip_dir("c.rs", ""), Some("c.rs"));
    }
}
