//! Batched writer for index upsert / remove. One `IndexWriter` per scanner worker; the
//! scanner commits each file's work atomically so a crash mid-scan leaves the index in
//! a consistent (just slightly stale) state.

use fjall::{Keyspace, OwnedWriteBatch, Slice};

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
    /// Resident bytes staged into `batch` so far, as modelled by [`staged_entry_bytes`]. Fjall
    /// keeps every staged entry resident until `commit`, so this — not the number of *files*
    /// staged — is what actually bounds a writer's live memory: one machine-generated source file
    /// can stage tens of thousands of entries, and a chunk's BM25 posting list is unbounded in the
    /// same way. Callers batch against [`Self::staged_bytes`]; see
    /// `scanner_index_batch::WorkerIndexBatch`.
    staged_bytes: u64,
    /// Test-only fault seam: make the next [`Self::upsert_file`] stage its deletes and inserts and
    /// *then* fail. The real failures have exactly that shape — a fjall read error inside
    /// `stage_deletes_for`, an `rmp_serde` encode error inside `stage_inserts_for` — and neither is
    /// reachable from a test, so without this seam the caller's "bytes staged by a failed upsert
    /// still count" invariant cannot be exercised at all.
    #[cfg(test)]
    pub(crate) fail_after_staging: bool,
}

/// Fixed cost of one entry staged in a fjall write batch, independent of its payload.
///
/// Derived from **fjall 3.1.10** (re-check on upgrade): `WriteBatch` accumulates into a
/// `Vec<Item>`, and `batch::item::Item` is
/// `{ keyspace: Keyspace(Arc<KeyspaceInner>) = 8, key: UserKey = Slice = 24, value: UserValue = 24,
/// value_type: ValueType = 1 }` → 57 bytes, rounded to 64 by the 8-byte alignment the `Arc` and the
/// `Slice`'s interior pointer impose. Every entry pays it, tombstones included.
const STAGED_ITEM_BYTES: u64 = 64;

/// Longest payload byteview stores *inside* the 24-byte `Slice` rather than on the heap
/// (`byteview::INLINE_SIZE` for 64-bit targets, **byteview 0.10.2** via lsm-tree 3.1.10). Payloads
/// this short are already covered by [`STAGED_ITEM_BYTES`] and cost nothing extra.
const SLICE_INLINE_MAX: usize = 20;

/// Header byteview prepends to every heap-allocated payload: `HeapAllocationHeader { ref_count:
/// AtomicU64 }` (byteview 0.10.2).
const SLICE_HEAP_HEADER: u64 = 8;

/// Resident bytes one staged entry costs, modelled on the types above rather than on the payload
/// alone.
///
/// Charging `key.len() + value.len()` — what this did before — undercounts by the whole
/// per-entry `Item` slot, which is fine for a 300-byte msgpack `Symbol` but is the *majority* of
/// the cost for the entries the scanner stages most of: the key-only secondary-index entries
/// (`symbols_by_name`, `calls_by_callee`, `imports_by_*`, `implementations_*`, `refs_by_*`) and the
/// BM25 postings, whose value is 8 bytes. Under the old accounting a 64 MiB budget of key+value
/// bytes was 130-160 MiB resident on a posting-dense scan — the exact workload the budget exists to
/// bound.
///
/// This is a deliberate **lower** bound on two counts, both allocator- or growth-dependent and so
/// not worth pretending to model: the system allocator rounds each heap block up to a size class,
/// and `Vec<Item>` grows by doubling, so up to another [`STAGED_ITEM_BYTES`] per entry can be
/// reserved-but-unwritten.
fn staged_entry_bytes(key_len: usize, value_len: usize) -> u64 {
    fn payload(len: usize) -> u64 {
        if len <= SLICE_INLINE_MAX {
            0
        } else {
            SLICE_HEAP_HEADER + len as u64
        }
    }
    STAGED_ITEM_BYTES + payload(key_len) + payload(value_len)
}

/// Stage one insert, charging its resident cost to `staged_bytes`.
///
/// Free-standing rather than a `&mut self` method so a caller can hold disjoint borrows of the
/// writer's fields — `&db.<partition>` alongside `&mut batch` — which a method taking `&mut self`
/// plus `&self.db.<partition>` would reject.
fn stage_insert<K: Into<Slice>, V: Into<Slice>>(
    batch: &mut OwnedWriteBatch,
    staged_bytes: &mut u64,
    partition: &Keyspace,
    key: K,
    value: V,
) {
    let key = key.into();
    let value = value.into();
    *staged_bytes += staged_entry_bytes(key.len(), value.len());
    batch.insert(partition, key, value);
}

/// Stage one delete. `WriteBatch::remove` stages a full `Item` carrying an empty value, so a
/// tombstone costs everything an insert does bar the value payload.
fn stage_remove<K: Into<Slice>>(batch: &mut OwnedWriteBatch, staged_bytes: &mut u64, partition: &Keyspace, key: K) {
    let key = key.into();
    *staged_bytes += staged_entry_bytes(key.len(), 0);
    batch.remove(partition, key);
}

impl IndexWriter {
    pub(super) fn new(db: IndexDb) -> Self {
        let batch = db.db.batch();
        Self {
            db,
            batch,
            staged_bytes: 0,
            #[cfg(test)]
            fail_after_staging: false,
        }
    }

    /// Resident bytes staged into the current batch, per [`staged_entry_bytes`]. There is no reset:
    /// [`Self::commit`] consumes the writer, so the counter dies with the batch it describes and
    /// the next batch starts a fresh writer at zero.
    pub fn staged_bytes(&self) -> u64 {
        self.staged_bytes
    }

    /// Replace the index entries for `rel` with those derived from `l1` (and optionally
    /// `l2`). Reads the existing per-file entries first to compute their secondary-index
    /// keys for deletion, then stages the fresh inserts in the same batch. Atomic.
    pub fn upsert_file(&mut self, rel: &RelPath, l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<(), IndexError> {
        self.stage_deletes_for(rel)?;
        self.stage_inserts_for(rel, l1)?;
        if let Some(l2) = l2 {
            self.stage_call_inserts(rel, l2)?;
        }
        #[cfg(test)]
        if self.fail_after_staging {
            return Err(IndexError::Io {
                path: rel.to_path_buf(),
                source: std::io::Error::other("injected upsert failure after staging"),
            });
        }
        Ok(())
    }

    /// Drop every index entry for `rel`. Used when a file is removed from the scan set.
    pub fn remove_file(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        self.stage_deletes_for(rel)?;
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        stage_remove(batch, staged_bytes, &db.grep_bloom, rel.as_bytes().to_vec());
        Ok(())
    }

    /// Replace `rel`'s grep bloom row with `row` (built by [`super::grep_bloom::build_row`] from the
    /// bytes the scanner already holds). Keyed by path alone, so the insert overwrites the previous
    /// row and no delete is staged. Counted against the staged-byte budget like every other entry.
    pub fn upsert_grep_bloom(&mut self, rel: &RelPath, row: Vec<u8>) {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        stage_insert(batch, staged_bytes, &db.grep_bloom, rel.as_bytes().to_vec(), row);
    }

    /// Replace the resolved-reference edges whose *use* is in `use_rel` with those derived from
    /// `refs` (the file's resolution facts). Deletes the file's existing edges first (keyed by
    /// use file, O(prefix)), then inserts each intra-file edge into both `refs_by_def` (keyed by
    /// the defining site → `find_references`) and `refs_by_path` (keyed by the use site →
    /// `goto_definition`). Atomic within the batch. Cross-file edges are staged by the resolve
    /// pass separately once import resolution lands.
    pub fn upsert_resolved_file(&mut self, use_rel: &RelPath, refs: &FileResolvedRefs) -> Result<(), IndexError> {
        self.stage_resolved_deletes_for(use_rel)?;
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        for edge in &refs.intra {
            stage_insert(
                batch,
                staged_bytes,
                &db.refs_by_def,
                keys::ref_by_def(use_rel, edge.def_start, use_rel, edge.use_start),
                Vec::<u8>::new(),
            );
            stage_insert(
                batch,
                staged_bytes,
                &db.refs_by_path,
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
    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    pub fn upsert_cross_file_edge(
        &mut self,
        def_rel: &RelPath,
        def_start: u32,
        use_rel: &RelPath,
        use_start: u32,
    ) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        stage_insert(
            batch,
            staged_bytes,
            &db.refs_by_def,
            keys::ref_by_def(def_rel, def_start, use_rel, use_start),
            Vec::<u8>::new(),
        );
        stage_insert(
            batch,
            staged_bytes,
            &db.refs_by_path,
            keys::ref_by_path(use_rel, use_start, def_rel, def_start),
            Vec::<u8>::new(),
        );
        Ok(())
    }

    /// Flush this batch to disk atomically. Consumes the writer.
    ///
    /// `WriteBatch::commit` journals every staged item, then applies it to the memtables and runs
    /// fjall's own `check_memtable_rotate` + `local_backpressure` for each affected keyspace — so
    /// the commit cadence is also how often the writer *hears* fjall asking it to stall. Batching
    /// by bytes rather than by file count samples that signal an order of magnitude more often on
    /// entry-dense files, and bounds the journal write burst to the same budget.
    pub fn commit(self) -> Result<(), IndexError> {
        self.batch.commit()?;
        Ok(())
    }

    fn stage_deletes_for(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let path_prefix = keys::symbols_by_path_prefix(rel);
        let mut found_symbols: Vec<(Vec<u8>, Symbol)> = Vec::new();
        for guard in db.symbols_by_path.prefix(path_prefix) {
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
            stage_remove(batch, staged_bytes, &db.symbols_by_path, path_key);
            if let Some(name_key) = keys::symbol_by_name(&sym.name, sym.kind, rel, sym.start_byte) {
                stage_remove(batch, staged_bytes, &db.symbols_by_name, name_key);
            }
        }

        let call_path_prefix = keys::calls_by_path_prefix(rel);
        let mut found_calls: Vec<(Vec<u8>, crate::extract::Call)> = Vec::new();
        for guard in db.calls_by_path.prefix(call_path_prefix) {
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
            stage_remove(batch, staged_bytes, &db.calls_by_path, path_key);
            if let Some(callee_key) = keys::call_by_callee(&call.callee, rel, call.start_byte) {
                stage_remove(batch, staged_bytes, &db.calls_by_callee, callee_key);
            }
        }

        let imp_path_prefix = keys::imports_by_path_prefix(rel);
        let mut found_imports: Vec<(Vec<u8>, String, u32)> = Vec::new();
        for guard in db.imports_by_path.prefix(imp_path_prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_, module, start_byte)) = keys::parse_import_by_path(&k) {
                found_imports.push(((*k).to_vec(), module, start_byte));
            }
        }
        for (path_key, module, start_byte) in found_imports {
            stage_remove(batch, staged_bytes, &db.imports_by_path, path_key);
            if let Some(module_key) = keys::import_by_module(&module, rel, start_byte) {
                stage_remove(batch, staged_bytes, &db.imports_by_module, module_key);
            }
        }

        let impl_path_prefix = keys::impls_by_path_prefix(rel);
        let mut found_impls: Vec<(Vec<u8>, String, String, u32)> = Vec::new();
        for guard in db.implementations_by_path.prefix(impl_path_prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_, trait_name, impl_type, start_byte)) = keys::parse_impl_by_path(&k) {
                found_impls.push(((*k).to_vec(), trait_name, impl_type, start_byte));
            }
        }
        for (path_key, trait_name, impl_type, start_byte) in found_impls {
            stage_remove(batch, staged_bytes, &db.implementations_by_path, path_key);
            if let Some(trait_key) = keys::impl_by_trait(&trait_name, &impl_type, rel, start_byte) {
                stage_remove(batch, staged_bytes, &db.implementations_by_trait, trait_key);
            }
        }
        Ok(())
    }

    /// Stage deletes for every resolved edge whose use is in `use_rel`. Scans `refs_by_path`
    /// under the file prefix, reconstructs each companion `refs_by_def` key, and removes both.
    fn stage_resolved_deletes_for(&mut self, use_rel: &RelPath) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let prefix = keys::refs_by_path_prefix(use_rel);
        let mut found: Vec<(Vec<u8>, RelPath, u32, u32)> = Vec::new();
        for guard in db.refs_by_path.prefix(prefix) {
            let (k, _) = guard.into_inner()?;
            if let Some((_use_path, use_start, def_path, def_start)) = keys::parse_ref_by_path(&k) {
                found.push(((*k).to_vec(), def_path, def_start, use_start));
            }
        }
        for (path_key, def_path, def_start, use_start) in found {
            stage_remove(batch, staged_bytes, &db.refs_by_path, path_key);
            stage_remove(
                batch,
                staged_bytes,
                &db.refs_by_def,
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
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let prefix = keys::code_bm25_by_path_prefix(rel);
        let mut found: Vec<(Vec<u8>, String, Vec<String>)> = Vec::new();
        for guard in db.code_bm25_by_path.prefix(prefix) {
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
            stage_remove(batch, staged_bytes, &db.code_bm25_by_path, path_key);
            for term in terms {
                if let Some(posting_key) = keys::code_bm25_posting(&term, &chunk_id) {
                    stage_remove(batch, staged_bytes, &db.code_bm25_postings, posting_key);
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
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        for posting in postings {
            for (term, tf) in &posting.terms {
                if let Some(posting_key) = keys::code_bm25_posting(term, &posting.chunk_id) {
                    stage_insert(
                        batch,
                        staged_bytes,
                        &db.code_bm25_postings,
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
            stage_insert(
                batch,
                staged_bytes,
                &db.code_bm25_by_path,
                keys::code_bm25_by_path(rel, &posting.chunk_id),
                value,
            );
        }
        Ok(())
    }

    /// Stage the L2 call sites for `rel` into both call partitions.
    fn stage_call_inserts(&mut self, rel: &RelPath, l2: &FileMapL2) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        // Before the keys are staged: the dictionary must never miss a name that has a key.
        db.callee_names.record(l2.calls.iter().map(|call| call.callee.as_str()));
        for call in &l2.calls {
            let path_key = keys::call_by_path(rel, call.start_byte);
            let value = rmp_serde::to_vec_named(call)?;
            stage_insert(batch, staged_bytes, &db.calls_by_path, path_key, value);
            if let Some(callee_key) = keys::call_by_callee(&call.callee, rel, call.start_byte) {
                stage_insert(batch, staged_bytes, &db.calls_by_callee, callee_key, Vec::<u8>::new());
            } else {
                tracing::debug!(
                    path = %rel,
                    callee_len = call.callee.len(),
                    "index: callee name exceeds 64 KiB — skipping calls_by_callee entry"
                );
            }
        }
        Ok(())
    }

    fn stage_inserts_for(&mut self, rel: &RelPath, l1: &FileMapL1) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        for sym in &l1.symbols {
            let path_key = keys::symbol_by_path(rel, sym.start_byte);
            let value = rmp_serde::to_vec_named(sym)?;
            stage_insert(batch, staged_bytes, &db.symbols_by_path, path_key, value);
            if let Some(name_key) = keys::symbol_by_name(&sym.name, sym.kind, rel, sym.start_byte) {
                stage_insert(batch, staged_bytes, &db.symbols_by_name, name_key, Vec::<u8>::new());
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
                        stage_insert(batch, staged_bytes, &db.imports_by_module, module_key, Vec::<u8>::new());
                        stage_insert(batch, staged_bytes, &db.imports_by_path, path_key, Vec::<u8>::new());
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
        // Before the keys are staged: the dictionary must never miss a name that has a key.
        db.trait_names
            .record(l1.implementations.iter().map(|imp| imp.trait_name.as_str()));
        for imp in &l1.implementations {
            match (
                keys::impl_by_trait(&imp.trait_name, &imp.impl_type, rel, imp.start_byte),
                keys::impl_by_path(rel, &imp.trait_name, &imp.impl_type, imp.start_byte),
            ) {
                (Some(trait_key), Some(path_key)) => {
                    stage_insert(
                        batch,
                        staged_bytes,
                        &db.implementations_by_trait,
                        trait_key,
                        Vec::<u8>::new(),
                    );
                    stage_insert(
                        batch,
                        staged_bytes,
                        &db.implementations_by_path,
                        path_key,
                        Vec::<u8>::new(),
                    );
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
#[path = "writer_tests.rs"]
mod tests;
