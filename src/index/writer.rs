//! Batched writer for index upsert / remove. One `IndexWriter` per scanner worker; the
//! scanner commits each file's work atomically so a crash mid-scan leaves the index in
//! a consistent (just slightly stale) state.

use fjall::OwnedWriteBatch;

use super::keys;
use super::{IndexDb, IndexError};
use crate::extract::{FileMapL1, FileMapL2, Symbol};
use crate::path::RelPath;

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
    pub fn upsert_file(
        &mut self,
        rel: &RelPath,
        l1: &FileMapL1,
        l2: Option<&FileMapL2>,
    ) -> Result<(), IndexError> {
        self.stage_deletes_for(rel)?;
        self.stage_inserts_for(rel, l1, l2)?;
        Ok(())
    }

    /// Drop every index entry for `rel`. Used when a file is removed from the scan set.
    pub fn remove_file(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        self.stage_deletes_for(rel)
    }

    /// Flush this batch to disk atomically. Consumes the writer.
    pub fn commit(self) -> Result<(), IndexError> {
        self.batch.commit()?;
        Ok(())
    }

    fn stage_deletes_for(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        // Symbols: scan symbols_by_path under this file's prefix, decode each Symbol to
        // derive the symbols_by_name key, stage both deletes.
        let path_prefix = keys::symbols_by_path_prefix(rel);
        let mut found_symbols: Vec<(Vec<u8>, Symbol)> = Vec::new();
        for guard in self.db.symbols_by_path.prefix(path_prefix) {
            let (k, v) = guard.into_inner()?;
            let sym: Symbol = rmp_serde::from_slice(&v)?;
            found_symbols.push(((*k).to_vec(), sym));
        }
        for (path_key, sym) in found_symbols {
            let name_key = keys::symbol_by_name(&sym.name, sym.kind, rel, sym.start_byte);
            self.batch.remove(&self.db.symbols_by_path, path_key);
            self.batch.remove(&self.db.symbols_by_name, name_key);
        }

        // Calls: scan calls_by_path under this file's prefix, decode each Call to derive
        // the calls_by_callee key, stage both deletes.
        let call_path_prefix = keys::calls_by_path_prefix(rel);
        let mut found_calls: Vec<(Vec<u8>, crate::extract::Call)> = Vec::new();
        for guard in self.db.calls_by_path.prefix(call_path_prefix) {
            let (k, v) = guard.into_inner()?;
            let call: crate::extract::Call = rmp_serde::from_slice(&v)?;
            found_calls.push(((*k).to_vec(), call));
        }
        for (path_key, call) in found_calls {
            let callee_key = keys::call_by_callee(&call.callee, rel, call.start_byte);
            self.batch.remove(&self.db.calls_by_path, path_key);
            self.batch.remove(&self.db.calls_by_callee, callee_key);
        }

        // Imports: we don't store a per-path entry (the secondary index is by module). We
        // walk by_module's full range and filter for rel-match — expensive for hot files.
        // Trade-off: per-file scans on small repos are fast; large repos eat this cost.
        // Could optimize with a separate imports_by_path partition later if it bites.
        let mut to_remove: Vec<Vec<u8>> = Vec::new();
        for guard in self.db.imports_by_module.iter() {
            let (k, _) = guard.into_inner()?;
            if let Some((_, candidate_rel, _)) = keys::parse_import_by_module(&k)
                && candidate_rel == *rel
            {
                to_remove.push((*k).to_vec());
            }
        }
        for k in to_remove {
            self.batch.remove(&self.db.imports_by_module, k);
        }
        Ok(())
    }

    fn stage_inserts_for(
        &mut self,
        rel: &RelPath,
        l1: &FileMapL1,
        l2: Option<&FileMapL2>,
    ) -> Result<(), IndexError> {
        for sym in &l1.symbols {
            let path_key = keys::symbol_by_path(rel, sym.start_byte);
            let name_key = keys::symbol_by_name(&sym.name, sym.kind, rel, sym.start_byte);
            let value = rmp_serde::to_vec_named(sym)?;
            self.batch.insert(&self.db.symbols_by_path, path_key, value);
            self.batch
                .insert(&self.db.symbols_by_name, name_key, Vec::<u8>::new());
        }
        for imp in &l1.imports {
            if let Some(module) = &imp.module {
                let key = keys::import_by_module(module, rel, imp.start_byte);
                self.batch
                    .insert(&self.db.imports_by_module, key, Vec::<u8>::new());
            }
        }
        if let Some(l2) = l2 {
            for call in &l2.calls {
                let path_key = keys::call_by_path(rel, call.start_byte);
                let callee_key = keys::call_by_callee(&call.callee, rel, call.start_byte);
                let value = rmp_serde::to_vec_named(call)?;
                self.batch.insert(&self.db.calls_by_path, path_key, value);
                self.batch
                    .insert(&self.db.calls_by_callee, callee_key, Vec::<u8>::new());
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
            assert_eq!(
                callee, "spawn",
                "prefix scan must not bleed into spawn_blocking"
            );
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

        // Prefix scan for "os" should NOT hit "os.path" thanks to length-prefixing.
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
}
