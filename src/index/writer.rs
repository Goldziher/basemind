//! Batched writer for index upsert / remove. One `IndexWriter` per scanner worker; the
//! scanner commits each file's work atomically so a crash mid-scan leaves the index in
//! a consistent (just slightly stale) state.

use fjall::OwnedWriteBatch;

use super::keys;
use super::{IndexDb, IndexError};
use crate::extract::{FileMapL1, FileMapL2, Symbol};
use crate::intel::model::FileResolvedRefs;
use crate::path::RelPath;
#[cfg(feature = "code-search")]
use crate::search::bm25::ChunkPosting;

pub struct IndexWriter {
    db: IndexDb,
    batch: OwnedWriteBatch,
}

impl IndexWriter {
    pub(super) fn new(db: IndexDb) -> Self {
        let batch = db.db.batch();
        Self { db, batch }
    }

    /// Replace the index entries for `rel` with those derived from `l1` (and optionally
    /// `l2`). Reads the existing per-file entries first to compute their secondary-index
    /// keys for deletion, then stages the fresh inserts in the same batch. Atomic.
    pub fn upsert_file(&mut self, rel: &RelPath, l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<(), IndexError> {
        self.stage_deletes_for(rel)?;
        self.stage_inserts_for(rel, l1, l2)?;
        Ok(())
    }

    /// Drop every index entry for `rel`. Used when a file is removed from the scan set.
    pub fn remove_file(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        self.stage_deletes_for(rel)
    }

    /// Replace the resolved-reference edges whose *use* is in `use_rel` with those derived from
    /// `refs` (the file's resolution facts). Deletes the file's existing edges first (keyed by
    /// use file, O(prefix)), then inserts each intra-file edge into both `refs_by_def` (keyed by
    /// the defining site → `find_references`) and `refs_by_path` (keyed by the use site →
    /// `goto_definition`). Atomic within the batch. Cross-file edges are staged by the resolve
    /// pass separately once import resolution lands.
    pub fn upsert_resolved_file(&mut self, use_rel: &RelPath, refs: &FileResolvedRefs) -> Result<(), IndexError> {
        self.stage_resolved_deletes_for(use_rel)?;
        for edge in &refs.intra {
            self.batch.insert(
                &self.db.refs_by_def,
                keys::ref_by_def(use_rel, edge.def_start, use_rel, edge.use_start),
                Vec::<u8>::new(),
            );
            self.batch.insert(
                &self.db.refs_by_path,
                keys::ref_by_path(use_rel, edge.use_start, use_rel, edge.def_start),
                Vec::<u8>::new(),
            );
        }
        Ok(())
    }

    /// Drop every resolved edge whose use is in `use_rel`. Used when a file leaves the scan set.
    pub fn remove_resolved_file(&mut self, use_rel: &RelPath) -> Result<(), IndexError> {
        self.stage_resolved_deletes_for(use_rel)
    }

    /// Replace the BM25 keyword postings for `rel`'s chunks with those in `postings`. Reads the
    /// file's existing forward entries first to derive the `code_bm25_postings` keys for deletion,
    /// then stages the fresh postings in the same batch. Atomic. Mirrors the `calls_by_path` →
    /// `calls_by_callee` dual-partition pattern for the code-search keyword lane.
    #[cfg(feature = "code-search")]
    pub fn upsert_bm25_file(&mut self, rel: &RelPath, postings: &[ChunkPosting]) -> Result<(), IndexError> {
        self.stage_bm25_deletes_for(rel)?;
        self.stage_bm25_inserts_for(rel, postings)?;
        Ok(())
    }

    /// Drop every BM25 posting for `rel`'s chunks. Used when a source file leaves the scan set.
    #[cfg(feature = "code-search")]
    pub fn remove_bm25_file(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        self.stage_bm25_deletes_for(rel)
    }

    /// Stage a single CROSS-FILE resolved edge: the use in `use_rel` binds to a definition in a
    /// *different* file `def_rel` (`def_rel != use_rel`) — an importer's binding stitched to the
    /// matching export in its resolved target module. Inserts into both `refs_by_def` (keyed by
    /// the defining site → `find_references`) and `refs_by_path` (keyed by the use site →
    /// `goto_definition`), mirroring the intra-file staging in [`Self::upsert_resolved_file`].
    ///
    /// Idempotency invariant: unlike `upsert_resolved_file`, this stages **no delete**. Every
    /// cross-file edge is keyed on its *use* file in `refs_by_path`, so
    /// [`Self::stage_resolved_deletes_for`] — invoked by `upsert_resolved_file` /
    /// `remove_resolved_file` when the importer is re-resolved earlier in the same resolve pass —
    /// has already purged the previous scan's cross-file edges for that importer. The cross-file
    /// join therefore runs *after* every importer's per-file upsert, so the importer's slate is
    /// clean before these inserts land, and a re-scan does not accumulate stale edges.
    #[cfg(feature = "code-intel-js")]
    pub fn upsert_cross_file_edge(
        &mut self,
        def_rel: &RelPath,
        def_start: u32,
        use_rel: &RelPath,
        use_start: u32,
    ) -> Result<(), IndexError> {
        self.batch.insert(
            &self.db.refs_by_def,
            keys::ref_by_def(def_rel, def_start, use_rel, use_start),
            Vec::<u8>::new(),
        );
        self.batch.insert(
            &self.db.refs_by_path,
            keys::ref_by_path(use_rel, use_start, def_rel, def_start),
            Vec::<u8>::new(),
        );
        Ok(())
    }

    /// Flush this batch to disk atomically. Consumes the writer.
    pub fn commit(self) -> Result<(), IndexError> {
        self.batch.commit()?;
        Ok(())
    }

    fn stage_deletes_for(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        let path_prefix = keys::symbols_by_path_prefix(rel);
        let mut found_symbols: Vec<(Vec<u8>, Symbol)> = Vec::new();
        for guard in self.db.symbols_by_path.prefix(path_prefix) {
            let (k, v) = guard.into_inner()?;
            match rmp_serde::from_slice::<Symbol>(&v) {
                Ok(sym) => found_symbols.push(((*k).to_vec(), sym)),
                Err(e) => {
                    tracing::warn!(
                        path = %rel,
                        error = %e,
                        "index: failed to decode Symbol blob during delete staging — skipping entry"
                    );
                }
            }
        }
        for (path_key, sym) in found_symbols {
            self.batch.remove(&self.db.symbols_by_path, path_key);
            if let Some(name_key) = keys::symbol_by_name(&sym.name, sym.kind, rel, sym.start_byte) {
                self.batch.remove(&self.db.symbols_by_name, name_key);
            }
        }

        let call_path_prefix = keys::calls_by_path_prefix(rel);
        let mut found_calls: Vec<(Vec<u8>, crate::extract::Call)> = Vec::new();
        for guard in self.db.calls_by_path.prefix(call_path_prefix) {
            let (k, v) = guard.into_inner()?;
            match rmp_serde::from_slice::<crate::extract::Call>(&v) {
                Ok(call) => found_calls.push(((*k).to_vec(), call)),
                Err(e) => {
                    tracing::warn!(
                        path = %rel,
                        error = %e,
                        "index: failed to decode Call blob during delete staging — skipping entry"
                    );
                }
            }
        }
        for (path_key, call) in found_calls {
            self.batch.remove(&self.db.calls_by_path, path_key);
            if let Some(callee_key) = keys::call_by_callee(&call.callee, rel, call.start_byte) {
                self.batch.remove(&self.db.calls_by_callee, callee_key);
            }
        }

        let imp_path_prefix = keys::imports_by_path_prefix(rel);
        let mut found_imports: Vec<(Vec<u8>, String, u32)> = Vec::new();
        for guard in self.db.imports_by_path.prefix(imp_path_prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_, module, start_byte)) = keys::parse_import_by_path(&k) {
                found_imports.push(((*k).to_vec(), module, start_byte));
            }
        }
        for (path_key, module, start_byte) in found_imports {
            self.batch.remove(&self.db.imports_by_path, path_key);
            if let Some(module_key) = keys::import_by_module(&module, rel, start_byte) {
                self.batch.remove(&self.db.imports_by_module, module_key);
            }
        }

        let impl_path_prefix = keys::impls_by_path_prefix(rel);
        let mut found_impls: Vec<(Vec<u8>, String, String, u32)> = Vec::new();
        for guard in self.db.implementations_by_path.prefix(impl_path_prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_, trait_name, impl_type, start_byte)) = keys::parse_impl_by_path(&k) {
                found_impls.push(((*k).to_vec(), trait_name, impl_type, start_byte));
            }
        }
        for (path_key, trait_name, impl_type, start_byte) in found_impls {
            self.batch.remove(&self.db.implementations_by_path, path_key);
            if let Some(trait_key) = keys::impl_by_trait(&trait_name, &impl_type, rel, start_byte) {
                self.batch.remove(&self.db.implementations_by_trait, trait_key);
            }
        }
        Ok(())
    }

    /// Stage deletes for every resolved edge whose use is in `use_rel`. Scans `refs_by_path`
    /// under the file prefix, reconstructs each companion `refs_by_def` key, and removes both.
    fn stage_resolved_deletes_for(&mut self, use_rel: &RelPath) -> Result<(), IndexError> {
        let prefix = keys::refs_by_path_prefix(use_rel);
        let mut found: Vec<(Vec<u8>, RelPath, u32, u32)> = Vec::new();
        for guard in self.db.refs_by_path.prefix(prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_use_path, use_start, def_path, def_start)) = keys::parse_ref_by_path(&k) {
                found.push(((*k).to_vec(), def_path, def_start, use_start));
            }
        }
        for (path_key, def_path, def_start, use_start) in found {
            self.batch.remove(&self.db.refs_by_path, path_key);
            self.batch.remove(
                &self.db.refs_by_def,
                keys::ref_by_def(&def_path, def_start, use_rel, use_start),
            );
        }
        Ok(())
    }

    /// Stage deletes for every BM25 posting of `rel`'s chunks. Scans `code_bm25_by_path` under the
    /// file prefix; each forward value carries `doclen:u32_be ‖ msgpack(Vec<String> terms)`, so the
    /// companion `code_bm25_postings` keys are reconstructed from the decoded term list.
    #[cfg(feature = "code-search")]
    fn stage_bm25_deletes_for(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        let prefix = keys::code_bm25_by_path_prefix(rel);
        let mut found: Vec<(Vec<u8>, String, Vec<String>)> = Vec::new();
        for guard in self.db.code_bm25_by_path.prefix(prefix) {
            let (k, v) = guard.into_inner()?;
            let Some((_rel, chunk_id)) = keys::parse_code_bm25_by_path(&k) else {
                continue;
            };
            let terms: Vec<String> = if v.len() >= 4 {
                match rmp_serde::from_slice::<Vec<String>>(&v[4..]) {
                    Ok(terms) => terms,
                    Err(e) => {
                        tracing::warn!(
                            path = %rel,
                            error = %e,
                            "index: failed to decode BM25 term list during delete staging — skipping entry"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            found.push(((*k).to_vec(), chunk_id, terms));
        }
        for (path_key, chunk_id, terms) in found {
            self.batch.remove(&self.db.code_bm25_by_path, path_key);
            for term in terms {
                if let Some(posting_key) = keys::code_bm25_posting(&term, &chunk_id) {
                    self.batch.remove(&self.db.code_bm25_postings, posting_key);
                }
            }
        }
        Ok(())
    }

    /// Stage inserts for `rel`'s BM25 postings: one `code_bm25_postings` entry per `(term, chunk)`
    /// carrying `tf ‖ doclen`, plus one `code_bm25_by_path` forward entry per chunk carrying
    /// `doclen ‖ msgpack(terms)` so the next re-scan can delete these in O(prefix).
    #[cfg(feature = "code-search")]
    fn stage_bm25_inserts_for(&mut self, rel: &RelPath, postings: &[ChunkPosting]) -> Result<(), IndexError> {
        for posting in postings {
            for (term, tf) in &posting.terms {
                if let Some(posting_key) = keys::code_bm25_posting(term, &posting.chunk_id) {
                    self.batch.insert(
                        &self.db.code_bm25_postings,
                        posting_key,
                        keys::code_bm25_posting_value(*tf, posting.doclen),
                    );
                }
            }
            let term_names: Vec<&str> = posting.terms.iter().map(|(t, _)| t.as_str()).collect();
            let terms_bytes = rmp_serde::to_vec(&term_names)?;
            let mut value = Vec::with_capacity(4 + terms_bytes.len());
            value.extend_from_slice(&posting.doclen.to_be_bytes());
            value.extend_from_slice(&terms_bytes);
            self.batch.insert(
                &self.db.code_bm25_by_path,
                keys::code_bm25_by_path(rel, &posting.chunk_id),
                value,
            );
        }
        Ok(())
    }

    fn stage_inserts_for(&mut self, rel: &RelPath, l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<(), IndexError> {
        for sym in &l1.symbols {
            let path_key = keys::symbol_by_path(rel, sym.start_byte);
            let value = rmp_serde::to_vec_named(sym)?;
            self.batch.insert(&self.db.symbols_by_path, path_key, value);
            if let Some(name_key) = keys::symbol_by_name(&sym.name, sym.kind, rel, sym.start_byte) {
                self.batch.insert(&self.db.symbols_by_name, name_key, Vec::<u8>::new());
            } else {
                tracing::debug!(
                    path = %rel,
                    name_len = sym.name.len(),
                    "index: symbol name exceeds 64 KiB — skipping symbols_by_name entry"
                );
            }
        }
        for imp in &l1.imports {
            if let Some(module) = &imp.module {
                match (
                    keys::import_by_module(module, rel, imp.start_byte),
                    keys::import_by_path(rel, module, imp.start_byte),
                ) {
                    (Some(module_key), Some(path_key)) => {
                        self.batch
                            .insert(&self.db.imports_by_module, module_key, Vec::<u8>::new());
                        self.batch.insert(&self.db.imports_by_path, path_key, Vec::<u8>::new());
                    }
                    _ => {
                        tracing::debug!(
                            path = %rel,
                            module_len = module.len(),
                            "index: import module name exceeds 64 KiB — skipping imports index entries"
                        );
                    }
                }
            }
        }
        if let Some(l2) = l2 {
            for call in &l2.calls {
                let path_key = keys::call_by_path(rel, call.start_byte);
                let value = rmp_serde::to_vec_named(call)?;
                self.batch.insert(&self.db.calls_by_path, path_key, value);
                if let Some(callee_key) = keys::call_by_callee(&call.callee, rel, call.start_byte) {
                    self.batch
                        .insert(&self.db.calls_by_callee, callee_key, Vec::<u8>::new());
                } else {
                    tracing::debug!(
                        path = %rel,
                        callee_len = call.callee.len(),
                        "index: callee name exceeds 64 KiB — skipping calls_by_callee entry"
                    );
                }
            }
        }
        for imp in &l1.implementations {
            match (
                keys::impl_by_trait(&imp.trait_name, &imp.impl_type, rel, imp.start_byte),
                keys::impl_by_path(rel, &imp.trait_name, &imp.impl_type, imp.start_byte),
            ) {
                (Some(trait_key), Some(path_key)) => {
                    self.batch
                        .insert(&self.db.implementations_by_trait, trait_key, Vec::<u8>::new());
                    self.batch
                        .insert(&self.db.implementations_by_path, path_key, Vec::<u8>::new());
                }
                _ => {
                    tracing::debug!(
                        path = %rel,
                        trait_len = imp.trait_name.len(),
                        impl_len = imp.impl_type.len(),
                        "index: trait/impl-type name exceeds 64 KiB — skipping implementations index entries"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Call, FileMapL2, Import, SymbolKind};
    use tempfile::TempDir;

    fn fresh_db() -> (TempDir, IndexDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        (dir, db)
    }

    fn synthetic_l1(syms: &[(&str, SymbolKind, u32)]) -> FileMapL1 {
        FileMapL1 {
            schema_ver: crate::extract::SCHEMA_VER,
            language: "rust".to_string(),
            size_bytes: 0,
            had_errors: false,
            error_count: 0,
            symbols: syms
                .iter()
                .map(|(name, kind, start)| Symbol {
                    name: name.to_string(),
                    kind: *kind,
                    start_byte: *start,
                    end_byte: *start + 1,
                    start_row: 0,
                    start_col: 0,
                    signature: None,
                    decorators: Vec::new(),
                })
                .collect(),
            imports: Vec::new(),
            implementations: Vec::new(),
        }
    }

    #[test]
    fn upsert_and_query_symbols_by_name() {
        let (_d, db) = fresh_db();
        let mut w = db.writer();
        let rel = RelPath::from("src/a.rs");
        let l1 = synthetic_l1(&[("alpha", SymbolKind::Function, 0)]);
        w.upsert_file(&rel, &l1, None).unwrap();
        w.commit().unwrap();

        let prefix = keys::symbols_by_name_prefix("alpha");
        let mut hits = 0;
        for guard in db.symbols_by_name.prefix(prefix) {
            let (k, _) = guard.into_inner().unwrap();
            let (name, _, _, _) = keys::parse_symbol_by_name(&k).unwrap();
            assert_eq!(name, "alpha");
            hits += 1;
        }
        assert_eq!(hits, 1);
    }

    #[test]
    fn upsert_then_remove_clears_partitions() {
        let (_d, db) = fresh_db();
        let mut w = db.writer();
        let rel = RelPath::from("src/a.rs");
        let l1 = synthetic_l1(&[("alpha", SymbolKind::Function, 0)]);
        w.upsert_file(&rel, &l1, None).unwrap();
        w.commit().unwrap();

        let mut w = db.writer();
        w.remove_file(&rel).unwrap();
        w.commit().unwrap();

        assert!(
            db.symbols_by_path.iter().next().is_none(),
            "symbols_by_path should be empty after remove_file"
        );
        assert!(
            db.symbols_by_name.iter().next().is_none(),
            "symbols_by_name should be empty after remove_file"
        );
    }

    #[test]
    fn calls_index_round_trip() {
        let (_d, db) = fresh_db();
        let mut w = db.writer();
        let rel = RelPath::from("src/main.rs");
        let l1 = synthetic_l1(&[("main", SymbolKind::Function, 0)]);
        let l2 = FileMapL2 {
            schema_ver: crate::extract::SCHEMA_VER,
            language: "rust".to_string(),
            calls: vec![
                Call {
                    callee: "spawn".to_string(),
                    start_byte: 10,
                    end_byte: 15,
                    start_row: 0,
                    start_col: 0,
                },
                Call {
                    callee: "spawn".to_string(),
                    start_byte: 30,
                    end_byte: 35,
                    start_row: 0,
                    start_col: 0,
                },
                Call {
                    callee: "spawn_blocking".to_string(),
                    start_byte: 50,
                    end_byte: 64,
                    start_row: 0,
                    start_col: 0,
                },
            ],
            docs: Vec::new(),
        };
        w.upsert_file(&rel, &l1, Some(&l2)).unwrap();
        w.commit().unwrap();

        let prefix = keys::calls_by_callee_prefix("spawn");
        let mut spawn_hits = 0;
        for guard in db.calls_by_callee.prefix(prefix) {
            let (k, _) = guard.into_inner().unwrap();
            let (callee, _, _) = keys::parse_call_by_callee(&k).unwrap();
            assert_eq!(callee, "spawn", "prefix scan must not bleed into spawn_blocking");
            spawn_hits += 1;
        }
        assert_eq!(spawn_hits, 2);
    }

    #[test]
    fn imports_by_module_round_trip() {
        let (_d, db) = fresh_db();
        let mut w = db.writer();
        let rel = RelPath::from("src/foo.py");
        let mut l1 = synthetic_l1(&[]);
        l1.imports = vec![
            Import {
                module: Some("os".to_string()),
                raw: "import os".to_string(),
                start_byte: 0,
                end_byte: 9,
            },
            Import {
                module: Some("os.path".to_string()),
                raw: "import os.path".to_string(),
                start_byte: 10,
                end_byte: 24,
            },
        ];
        w.upsert_file(&rel, &l1, None).unwrap();
        w.commit().unwrap();

        let prefix = keys::imports_by_module_prefix("os");
        let mut os_hits = 0;
        for guard in db.imports_by_module.prefix(prefix) {
            let (k, _) = guard.into_inner().unwrap();
            let (module, _, _) = keys::parse_import_by_module(&k).unwrap();
            assert_eq!(module, "os");
            os_hits += 1;
        }
        assert_eq!(os_hits, 1, "prefix scan must isolate `os` from `os.path`");
    }

    fn synthetic_l1_with_impls(impls: &[(&str, &str, u32)]) -> FileMapL1 {
        let mut l1 = synthetic_l1(&[]);
        l1.implementations = impls
            .iter()
            .map(|(t, i, sb)| crate::extract::Implementation {
                trait_name: t.to_string(),
                impl_type: i.to_string(),
                start_byte: *sb,
                start_row: 0,
                start_col: 0,
            })
            .collect();
        l1
    }

    /// Iteration-3 dual-partition test for implementations. Mirrors
    /// `imports_by_path_roundtrip_and_dual_partition_consistency`: upsert two rows, verify
    /// both partitions have 2 entries; re-upsert with one row dropped, verify both
    /// partitions have 1 entry; remove the file, verify both partitions empty.
    #[test]
    fn implementations_dual_partition_consistency() {
        let (_d, db) = fresh_db();
        let rel = RelPath::from("src/foo.rs");

        let mut w = db.writer();
        w.upsert_file(
            &rel,
            &synthetic_l1_with_impls(&[("Display", "Foo", 0), ("Debug", "Foo", 10)]),
            None,
        )
        .unwrap();
        w.commit().unwrap();

        assert_eq!(db.implementations_by_trait.iter().count(), 2);
        assert_eq!(db.implementations_by_path.iter().count(), 2);

        let prefix = keys::impls_by_trait_prefix("Display");
        let mut display_hits = 0;
        for guard in db.implementations_by_trait.prefix(prefix) {
            let (k, _) = guard.into_inner().unwrap();
            let (trait_name, impl_type, back_rel, _) = keys::parse_impl_by_trait(&k).unwrap();
            assert_eq!(trait_name, "Display");
            assert_eq!(impl_type, "Foo");
            assert_eq!(back_rel, rel);
            display_hits += 1;
        }
        assert_eq!(display_hits, 1);

        let mut w = db.writer();
        w.upsert_file(&rel, &synthetic_l1_with_impls(&[("Display", "Foo", 0)]), None)
            .unwrap();
        w.commit().unwrap();

        assert_eq!(db.implementations_by_trait.iter().count(), 1);
        assert_eq!(db.implementations_by_path.iter().count(), 1);

        let mut w = db.writer();
        w.remove_file(&rel).unwrap();
        w.commit().unwrap();

        assert!(db.implementations_by_trait.iter().next().is_none());
        assert!(db.implementations_by_path.iter().next().is_none());
    }

    #[test]
    fn imports_by_path_roundtrip_and_dual_partition_consistency() {
        let (_d, db) = fresh_db();
        let mut w = db.writer();
        let rel = RelPath::from("src/foo.py");
        let mut l1 = synthetic_l1(&[]);
        l1.imports = vec![
            Import {
                module: Some("os".to_string()),
                raw: "import os".to_string(),
                start_byte: 0,
                end_byte: 9,
            },
            Import {
                module: Some("os.path".to_string()),
                raw: "import os.path".to_string(),
                start_byte: 10,
                end_byte: 24,
            },
        ];
        w.upsert_file(&rel, &l1, None).unwrap();
        w.commit().unwrap();

        assert_eq!(db.imports_by_module.iter().count(), 2);
        assert_eq!(db.imports_by_path.iter().count(), 2);

        let prefix = keys::imports_by_path_prefix(&rel);
        let mut path_hits = 0;
        for guard in db.imports_by_path.prefix(prefix) {
            let (k, _) = guard.into_inner().unwrap();
            let (back_rel, _, _) = keys::parse_import_by_path(&k).unwrap();
            assert_eq!(back_rel, rel);
            path_hits += 1;
        }
        assert_eq!(path_hits, 2);

        let mut l1 = synthetic_l1(&[]);
        l1.imports = vec![Import {
            module: Some("os".to_string()),
            raw: "import os".to_string(),
            start_byte: 0,
            end_byte: 9,
        }];
        let mut w = db.writer();
        w.upsert_file(&rel, &l1, None).unwrap();
        w.commit().unwrap();

        assert_eq!(db.imports_by_module.iter().count(), 1);
        assert_eq!(db.imports_by_path.iter().count(), 1);

        let mut w = db.writer();
        w.remove_file(&rel).unwrap();
        w.commit().unwrap();

        assert!(db.imports_by_module.iter().next().is_none());
        assert!(db.imports_by_path.iter().next().is_none());
    }

    /// Mixed oversized/normal upsert: the normal symbol must land in both partitions, the
    /// oversized symbol must land only in `symbols_by_path` (outline stays complete). No
    /// panic, no error propagated.
    #[test]
    fn oversized_identifier_skipped_gracefully() {
        let (_d, db) = fresh_db();
        let rel = RelPath::from("src/big.rs");
        let huge_name = "x".repeat(65536);
        let l1 = synthetic_l1(&[
            ("normal_fn", SymbolKind::Function, 0),
            (&huge_name, SymbolKind::Function, 100),
        ]);
        let mut w = db.writer();
        w.upsert_file(&rel, &l1, None).unwrap();
        w.commit().unwrap();

        assert_eq!(
            db.symbols_by_path.iter().count(),
            2,
            "both symbols must be in symbols_by_path"
        );
        assert_eq!(
            db.symbols_by_name.iter().count(),
            1,
            "only the normal symbol must be in symbols_by_name"
        );
        let prefix = keys::symbols_by_name_prefix("normal_fn");
        let hits: Vec<_> = db
            .symbols_by_name
            .prefix(prefix)
            .map(|g| g.into_inner().unwrap())
            .collect();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn resolved_edges_dual_partition_consistency() {
        use crate::intel::model::{FileResolvedRefs, ResolvedEdge};
        let (_d, db) = fresh_db();
        let rel = RelPath::from("src/app.ts");

        let mut refs = FileResolvedRefs::new("typescript");
        refs.intra = vec![
            ResolvedEdge {
                use_start: 100,
                use_end: 103,
                def_start: 4,
                def_end: 7,
            },
            ResolvedEdge {
                use_start: 200,
                use_end: 203,
                def_start: 4,
                def_end: 7,
            },
        ];
        let mut w = db.writer();
        w.upsert_resolved_file(&rel, &refs).unwrap();
        w.commit().unwrap();

        assert_eq!(db.refs_by_def.iter().count(), 2);
        assert_eq!(db.refs_by_path.iter().count(), 2);

        let mut uses: Vec<u32> = db
            .refs_by_def
            .prefix(keys::refs_by_def_prefix(&rel, 4))
            .map(|g| {
                let (k, _) = g.into_inner().unwrap();
                let (_dp, dstart, _up, ustart) = keys::parse_ref_by_def(&k).unwrap();
                assert_eq!(dstart, 4);
                ustart
            })
            .collect();
        uses.sort_unstable();
        assert_eq!(uses, vec![100, 200], "both uses must resolve to def@4");

        let defs: Vec<u32> = db
            .refs_by_path
            .prefix(keys::refs_by_use_prefix(&rel, 100))
            .map(|g| {
                let (k, _) = g.into_inner().unwrap();
                let (_up, ustart, _dp, dstart) = keys::parse_ref_by_path(&k).unwrap();
                assert_eq!(ustart, 100);
                dstart
            })
            .collect();
        assert_eq!(defs, vec![4], "use@100 must resolve to def@4");

        refs.intra.truncate(1);
        let mut w = db.writer();
        w.upsert_resolved_file(&rel, &refs).unwrap();
        w.commit().unwrap();
        assert_eq!(db.refs_by_def.iter().count(), 1);
        assert_eq!(db.refs_by_path.iter().count(), 1);

        let mut w = db.writer();
        w.remove_resolved_file(&rel).unwrap();
        w.commit().unwrap();
        assert!(db.refs_by_def.iter().next().is_none());
        assert!(db.refs_by_path.iter().next().is_none());
    }

    #[cfg(feature = "code-search")]
    #[test]
    fn bm25_dual_partition_consistency() {
        use crate::search::bm25::ChunkPosting;
        let (_d, db) = fresh_db();
        let rel = RelPath::from("src/foo.rs");

        let postings = vec![
            ChunkPosting {
                chunk_id: "h:0".to_string(),
                doclen: 3,
                terms: vec![("spawn".to_string(), 2), ("task".to_string(), 1)],
            },
            ChunkPosting {
                chunk_id: "h:1".to_string(),
                doclen: 1,
                terms: vec![("spawn".to_string(), 1)],
            },
        ];
        let mut w = db.writer();
        w.upsert_bm25_file(&rel, &postings).unwrap();
        w.commit().unwrap();

        assert_eq!(db.code_bm25_postings.iter().count(), 3);
        assert_eq!(db.code_bm25_by_path.iter().count(), 2);

        let mut spawn_docs: Vec<(String, u32, u32)> = db
            .code_bm25_postings
            .prefix(keys::code_bm25_postings_prefix("spawn"))
            .map(|g| {
                let (k, v) = g.into_inner().unwrap();
                let chunk_id = keys::parse_code_bm25_posting_chunk_id(&k).unwrap().to_string();
                let (tf, doclen) = keys::parse_code_bm25_posting_value(&v).unwrap();
                (chunk_id, tf, doclen)
            })
            .collect();
        spawn_docs.sort();
        assert_eq!(spawn_docs, vec![("h:0".to_string(), 2, 3), ("h:1".to_string(), 1, 1)]);

        let mut w = db.writer();
        w.upsert_bm25_file(
            &rel,
            &[ChunkPosting {
                chunk_id: "h:0".to_string(),
                doclen: 1,
                terms: vec![("spawn".to_string(), 1)],
            }],
        )
        .unwrap();
        w.commit().unwrap();
        assert_eq!(db.code_bm25_postings.iter().count(), 1);
        assert_eq!(db.code_bm25_by_path.iter().count(), 1);

        db.recompute_bm25_stats().unwrap();
        assert_eq!(db.bm25_stats(), Some((1, 1)));

        let mut w = db.writer();
        w.remove_bm25_file(&rel).unwrap();
        w.commit().unwrap();
        assert!(db.code_bm25_postings.iter().next().is_none());
        assert!(db.code_bm25_by_path.iter().next().is_none());
    }
}
