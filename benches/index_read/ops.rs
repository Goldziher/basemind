//! The measured operations, against the index layout as it is today: one store, and one Fjall
//! index, per worktree.
//!
//! This file is the only part of the benchmark that knows the layout. When worktrees of a group
//! come to share one index, [`Worktree`] changes to hold a view of that index; the corpus, the
//! queries and the reporting stay as they are, so the numbers remain comparable.
//!
//! The scan loops the MCP tools run (`scan_calls_by_name` in `src/mcp/helpers_calls_scan.rs`,
//! `scan_impls_fjall` in `src/mcp/helpers_impls.rs`, `resolved_callers` in
//! `src/mcp/helpers_calls.rs`) are crate-private, so each operation below repeats that loop over
//! the public `IndexView` methods the loop itself calls, with the same caps.

use std::ops::Bound;
use std::path::{Path, PathBuf};

use ahash::AHashMap;
use basemind::config::ConfigV1;
use basemind::extract::Call;
use basemind::index::grep_bloom::{GrepEntry, Needle};
use basemind::index::keys;
use basemind::index::view::{IndexView, Part};
use basemind::path::RelPath;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use memchr::memmem::Finder;

/// `SCAN_KEYS_EXAMINED_MAX` in `src/mcp/helpers_calls_scan.rs`.
const SCAN_KEYS_EXAMINED_MAX: usize = 200_000;
/// The floor of `scan_cap = limit * 8` in the same file.
const SCAN_CAP_FLOOR: usize = 2_000;

/// What one read did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpResult {
    /// Hits materialised on the page.
    pub hits: usize,
    /// Matches counted, up to the scan cap.
    pub total: usize,
    /// Index rows read to produce them.
    pub rows: usize,
}

/// A definition whose callers are asked for, resolved ahead of the timed loop the way the MCP tool
/// resolves it from its in-memory outline cache.
#[derive(Clone, Debug)]
pub struct Definition {
    pub path: RelPath,
    pub name: String,
    /// Identifier offsets the resolver may have recorded for it.
    pub def_starts: Vec<u32>,
}

pub struct Worktree {
    pub root: PathBuf,
    store: Store,
    grep_entries: Vec<GrepEntry>,
}

/// `identifier_is` in `src/mcp/helpers_refs_match.rs`: exact matching of a possibly qualified name.
fn identifier_is(identifier: &str, name: &str) -> bool {
    identifier
        .strip_suffix(name)
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with([':', '.', '>', '/', '\\']))
}

/// `trait_is` in the same file: generic arguments do not count.
fn trait_is(trait_name: &str, name: &str) -> bool {
    let bare = trait_name.split(['<', '(', '[']).next().unwrap_or(trait_name).trim();
    identifier_is(bare, name)
}

impl Worktree {
    /// Open the worktree's store and index its working tree.
    pub fn index(root: &Path) -> Self {
        let mut store = Store::open(root, VIEW_WORKING).expect("open store");
        let mut config = ConfigV1::with_defaults();
        config.documents.enabled = false;
        let report = scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan");
        assert_eq!(report.stats.extract_failed, 0, "synthetic sources must all extract");
        let mut grep_entries: Vec<GrepEntry> = store
            .index
            .files
            .iter()
            .map(|(rel, entry)| GrepEntry {
                rel: rel.clone(),
                hash_hex: entry.hash_hex.clone(),
                size_bytes: entry.size_bytes,
                mtime_ns: entry.mtime,
            })
            .collect();
        grep_entries.sort_by(|a, b| a.rel.cmp(&b.rel));
        Self {
            root: root.to_path_buf(),
            store,
            grep_entries,
        }
    }

    fn view(&self) -> impl IndexView + '_ {
        self.store.index_view().expect("a writer store holds its index")
    }

    /// Files the worktree indexed.
    pub fn file_count(&self) -> usize {
        self.store.index.files.len()
    }

    /// Content hash of every indexed file, for counting the group's distinct contents.
    pub fn content_hashes(&self) -> impl Iterator<Item = (&RelPath, &str)> {
        self.store
            .index
            .files
            .iter()
            .map(|(rel, entry)| (rel, entry.hash_hex.as_str()))
    }

    /// Directory holding this worktree's index and file map.
    pub fn cache_dir(&self) -> PathBuf {
        self.store.basemind_dir.clone()
    }

    /// `(path, start)` of every `calls_by_callee` row whose callee contains `name`. Not timed: it
    /// sizes the rows a content-keyed group index would hold for the name.
    pub fn callee_rows(&self, name: &str) -> Vec<(RelPath, u32)> {
        let finder = Finder::new(name.as_bytes());
        self.view()
            .callee_keys_matching(name, None, Bound::Unbounded)
            .filter_map(|row| row.ok())
            .filter_map(|(key, _)| keys::parse_call_by_callee(&key))
            .filter(|(callee, _, _)| finder.find(callee.as_bytes()).is_some())
            .map(|(_, rel, start)| (rel, start))
            .collect()
    }

    /// Resolve `name` in `path` to the offsets the resolver keys its edges by: the definition's
    /// identifier, found among the file's resolved edges and exports inside the symbol's span.
    pub fn definition(&self, path: &str, name: &str) -> Definition {
        let rel = RelPath::from(path);
        let outline = basemind::query::file_outline(&self.store, path).expect("outline of a generated file");
        let symbol = outline
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("{path} defines {name}"));
        let source = std::fs::read(self.root.join(path)).expect("read generated file");
        let names_it = |start: u32| {
            let start = start as usize;
            source.get(start..start + name.len()) == Some(name.as_bytes())
                && source
                    .get(start + name.len())
                    .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
        };
        let mut def_starts: Vec<u32> = Vec::new();
        if let Some(entry) = self.store.lookup(path)
            && let Ok(Some(refs)) = self.store.read_resolved_by_hex(&entry.hash_hex)
        {
            let candidates = refs
                .intra
                .iter()
                .map(|edge| edge.def_start)
                .chain(refs.exports.iter().map(|export| export.name_start));
            for start in candidates {
                if start >= symbol.start_byte
                    && start < symbol.end_byte
                    && names_it(start)
                    && !def_starts.contains(&start)
                {
                    def_starts.push(start);
                }
            }
        }
        Definition {
            path: rel,
            name: name.to_string(),
            def_starts,
        }
    }

    /// `code_refs` references: the `calls_by_callee` name scan, first page.
    pub fn references(&self, name: &str, exact: bool, limit: usize) -> OpResult {
        let view = self.view();
        let finder = Finder::new(name.as_bytes());
        let scan_cap = limit.saturating_mul(8).max(SCAN_CAP_FLOOR);
        let mut result = OpResult::default();
        for row in view.callee_keys_matching(name, None, Bound::Unbounded) {
            let Ok((key, _)) = row else {
                continue;
            };
            if result.rows >= SCAN_KEYS_EXAMINED_MAX {
                break;
            }
            result.rows += 1;
            let Some((callee, rel, start)) = keys::parse_call_by_callee(&key) else {
                continue;
            };
            if finder.find(callee.as_bytes()).is_none() || (exact && !identifier_is(&callee, name)) {
                continue;
            }
            result.total += 1;
            if result.hits < limit {
                // The page resolves each hit's line and column from its `calls_by_path` row.
                if let Ok(Some(value)) = view.get(Part::CallsByPath, &keys::call_by_path(&rel, start)) {
                    result.rows += 1;
                    std::hint::black_box(rmp_serde::from_slice::<Call>(&value).ok());
                }
                result.hits += 1;
            }
            if result.total >= scan_cap {
                break;
            }
        }
        result
    }

    /// `code_refs` callers: the exact name scan, then the resolved uses of the definition and the
    /// call sites of every file holding one.
    pub fn callers(&self, definition: &Definition, limit: usize) -> OpResult {
        let mut result = self.references(&definition.name, true, limit);
        let view = self.view();
        let mut uses: Vec<(RelPath, u32)> = Vec::new();
        for def_start in &definition.def_starts {
            for use_ref in view.references_to(&definition.path, *def_start) {
                result.rows += 1;
                if !uses.contains(&use_ref) {
                    uses.push(use_ref);
                }
            }
        }
        uses.sort_unstable();
        let probe_cap = limit.saturating_mul(8).max(SCAN_CAP_FLOOR);
        let mut proven = 0;
        for run in uses.chunk_by(|a, b| a.0 == b.0).take(probe_cap) {
            let prefix = keys::calls_by_path_prefix(&run[0].0);
            let mut calls: AHashMap<u32, Call> = AHashMap::new();
            for row in view.prefix(Part::CallsByPath, &prefix) {
                let Ok((_, value)) = row else {
                    continue;
                };
                result.rows += 1;
                if let Ok(call) = rmp_serde::from_slice::<Call>(&value) {
                    calls.insert(call.start_byte, call);
                }
            }
            proven += run.iter().filter(|(_, start)| calls.contains_key(start)).count();
        }
        result.total = result.total.max(proven);
        result
    }

    /// `code_refs` implementations: the `implementations_by_trait` name scan, first page.
    pub fn implementations(&self, trait_name: &str, limit: usize) -> OpResult {
        let view = self.view();
        let finder = Finder::new(trait_name.as_bytes());
        let scan_cap = limit.saturating_mul(8).max(SCAN_CAP_FLOOR);
        let mut result = OpResult::default();
        for row in view.trait_keys_matching(trait_name, None, Bound::Unbounded) {
            let Ok((key, _)) = row else {
                continue;
            };
            if result.rows >= SCAN_KEYS_EXAMINED_MAX {
                break;
            }
            result.rows += 1;
            let accepted = keys::parse_impl_by_trait(&key)
                .filter(|(found, _, _, _)| finder.find(found.as_bytes()).is_some() && trait_is(found, trait_name));
            if accepted.is_none() {
                continue;
            }
            result.total += 1;
            result.hits += usize::from(result.hits < limit);
            if result.total >= scan_cap {
                break;
            }
        }
        result
    }

    /// The exact symbol lookup behind the code-search exact lane.
    pub fn symbol_exact(&self, name: &str, cap: usize) -> OpResult {
        let symbols = self.view().symbols_by_name_lookup(name, cap);
        let exact = symbols.iter().filter(|(found, _, _, _)| found == name).count();
        OpResult {
            hits: exact,
            total: symbols.len(),
            rows: symbols.len(),
        }
    }

    /// Grep candidate selection: which files the trigram blooms cannot rule out for `pattern`.
    /// `hits` is the number of files a grep would still have to read.
    pub fn grep_candidates(&self, pattern: &str) -> OpResult {
        let needle = Needle::compile(pattern).expect("a literal pattern compiles to a needle");
        let skip = self.view().grep_skip_verdicts(&self.root, &self.grep_entries, &needle);
        let candidates = skip.iter().filter(|skipped| !**skipped).count();
        OpResult {
            hits: candidates,
            total: skip.len(),
            rows: skip.len(),
        }
    }
}
