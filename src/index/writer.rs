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

        // Calls: scan calls_by_path under this file's prefix, decode each Call to derive
        // the calls_by_callee key, stage both deletes.
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

        // Imports: prefix-scan imports_by_path for this file, derive the
        // imports_by_module key from each entry, stage both deletes. O(matches) instead
        // of the previous O(total imports) full-iter scan.
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

        // Implementations: prefix-scan implementations_by_path for this file, derive the
        // implementations_by_trait key from each entry, stage both deletes. Mirrors the
        // imports dual-partition pattern.
        let impl_path_prefix = keys::impls_by_path_prefix(rel);
        let mut found_impls: Vec<(Vec<u8>, String, String, u32)> = Vec::new();
        for guard in self.db.implementations_by_path.prefix(impl_path_prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_, trait_name, impl_type, start_byte)) = keys::parse_impl_by_path(&k) {
                found_impls.push(((*k).to_vec(), trait_name, impl_type, start_byte));
            }
        }
        for (path_key, trait_name, impl_type, start_byte) in found_impls {
            self.batch
                .remove(&self.db.implementations_by_path, path_key);
            if let Some(trait_key) = keys::impl_by_trait(&trait_name, &impl_type, rel, start_byte) {
                self.batch
                    .remove(&self.db.implementations_by_trait, trait_key);
            }
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
            let value = rmp_serde::to_vec_named(sym)?;
            // Always write the primary (by-path) entry so the outline stays complete.
            self.batch.insert(&self.db.symbols_by_path, path_key, value);
            // Secondary (by-name) entry is skipped silently for oversized identifiers.
            if let Some(name_key) = keys::symbol_by_name(&sym.name, sym.kind, rel, sym.start_byte) {
                self.batch
                    .insert(&self.db.symbols_by_name, name_key, Vec::<u8>::new());
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
                // Both partitions are secondary: skip both on oversized module names.
                match (
                    keys::import_by_module(module, rel, imp.start_byte),
                    keys::import_by_path(rel, module, imp.start_byte),
                ) {
                    (Some(module_key), Some(path_key)) => {
                        self.batch
                            .insert(&self.db.imports_by_module, module_key, Vec::<u8>::new());
                        self.batch
                            .insert(&self.db.imports_by_path, path_key, Vec::<u8>::new());
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
                // Always write the primary (by-path) entry.
                self.batch.insert(&self.db.calls_by_path, path_key, value);
                // Secondary (by-callee) entry is skipped silently for oversized callee names.
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
            // Both partitions are secondary: skip both on oversized trait/impl-type names.
            match (
                keys::impl_by_trait(&imp.trait_name, &imp.impl_type, rel, imp.start_byte),
                keys::impl_by_path(rel, &imp.trait_name, &imp.impl_type, imp.start_byte),
            ) {
                (Some(trait_key), Some(path_key)) => {
                    self.batch.insert(
                        &self.db.implementations_by_trait,
                        trait_key,
                        Vec::<u8>::new(),
                    );
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

        // Initial upsert with two impls.
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

        // Prefix scan for `Display` returns exactly one hit (length-prefix isolates Display
        // from any future DisplayFmt-style longer-name impls).
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

        // Re-upsert with the Debug impl dropped — upsert_file stages deletes for the
        // existing rows then inserts the fresh set in one batch.
        let mut w = db.writer();
        w.upsert_file(
            &rel,
            &synthetic_l1_with_impls(&[("Display", "Foo", 0)]),
            None,
        )
        .unwrap();
        w.commit().unwrap();

        assert_eq!(db.implementations_by_trait.iter().count(), 1);
        assert_eq!(db.implementations_by_path.iter().count(), 1);

        // Remove the file → both partitions empty.
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

        // Both partitions populated, both have 2 rows.
        assert_eq!(db.imports_by_module.iter().count(), 2);
        assert_eq!(db.imports_by_path.iter().count(), 2);

        // imports_by_path prefix scan returns both for this file.
        let prefix = keys::imports_by_path_prefix(&rel);
        let mut path_hits = 0;
        for guard in db.imports_by_path.prefix(prefix) {
            let (k, _) = guard.into_inner().unwrap();
            let (back_rel, _, _) = keys::parse_import_by_path(&k).unwrap();
            assert_eq!(back_rel, rel);
            path_hits += 1;
        }
        assert_eq!(path_hits, 2);

        // Re-upsert with one import dropped → only that one survives in BOTH partitions.
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

        // Remove the file → both partitions empty.
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
        // Must not panic.
        w.upsert_file(&rel, &l1, None).unwrap();
        w.commit().unwrap();

        // Both symbols appear in the primary partition (outlines complete).
        assert_eq!(
            db.symbols_by_path.iter().count(),
            2,
            "both symbols must be in symbols_by_path"
        );
        // Only the normal symbol appears in the secondary partition.
        assert_eq!(
            db.symbols_by_name.iter().count(),
            1,
            "only the normal symbol must be in symbols_by_name"
        );
        // Prefix scan finds the normal symbol.
        let prefix = keys::symbols_by_name_prefix("normal_fn");
        let hits: Vec<_> = db
            .symbols_by_name
            .prefix(prefix)
            .map(|g| g.into_inner().unwrap())
            .collect();
        assert_eq!(hits.len(), 1);
    }
}
