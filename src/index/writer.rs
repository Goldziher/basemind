//! Batched writer for index upsert / remove. One `IndexWriter` per scanner worker; the
//! scanner commits each file's work atomically so a crash mid-scan leaves the index in
//! a consistent (just slightly stale) state.

use fjall::{OwnedWriteBatch, Slice};

use super::scoped::Scoped;

use super::keys;
use super::writer_content::{Family, Fresh, FreshRow};
use super::{IndexDb, IndexError};
use crate::extract::{FileMapL1, FileMapL2};
use crate::intel::model::FileResolvedRefs;
use crate::path::RelPath;
#[cfg(feature = "code-search")]
use crate::search::bm25::ChunkPosting;

/// The symbol rows of a content: `symbols_by_path` and `symbols_by_name`.
fn symbol_family(db: &IndexDb) -> Family<'_> {
    Family {
        rows: &db.symbols_by_path,
        names: &db.symbols_by_name,
        names_of: super::writer_content::symbol_name_keys,
    }
}

/// The call rows of a content: `calls_by_path` and `calls_by_callee`.
fn call_family(db: &IndexDb) -> Family<'_> {
    Family {
        rows: &db.calls_by_path,
        names: &db.calls_by_callee,
        names_of: super::writer_content::callee_name_keys,
    }
}

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
    /// The batch inserts BM25 postings or owes the group a sweep, so its commit holds the group's
    /// BM25 gate shared.
    writes_postings: bool,
    /// The batch changes this view's BM25 forward rows, so its commit invalidates the live-key cache.
    changes_bm25_forward: bool,
    /// What the batch does to the BM25 forward rows, for an open full pass (see `bm25_pass`).
    #[cfg(feature = "code-search")]
    bm25_delta: super::bm25_pass::Bm25Delta,
    /// The batch already stages a fresh BM25 dirty token for the view.
    #[cfg(feature = "code-search")]
    bm25_dirty_marked: bool,
    /// The batch already stages the group's owed sweep.
    #[cfg(feature = "code-search")]
    bm25_sweep_owed: bool,
    /// The batch already stages the group's owed content sweep, so its commit holds the group gate
    /// shared (see [`super::group_sweep`]).
    content_sweep_owed: bool,
    /// BM25 keys this batch stages forward rows for, in flight until it commits or drops.
    #[cfg(feature = "code-search")]
    bm25_in_flight: super::bm25_live::InFlight,
    /// Test-only fault seam: make the next [`Self::upsert_file`] (or `upsert_bm25_file`) stage its
    /// deletes and inserts and *then* fail. The real failures have exactly that shape — a fjall read
    /// error inside `stage_deletes_for`, an `rmp_serde` encode error inside `stage_inserts_for` — and
    /// neither is reachable from a test, so without this seam the caller's "bytes staged by a failed
    /// upsert still count" invariant cannot be exercised at all.
    #[cfg(test)]
    pub(crate) fail_after_staging: bool,
    /// Test-only fault seam: fail the read of a file's BM25 forward rows after this many rows, the
    /// shape of a fjall read error partway through the prefix.
    #[cfg(all(test, feature = "code-search"))]
    pub(crate) fail_forward_read_after: Option<usize>,
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
pub(super) fn stage_insert<K: Into<Vec<u8>>, V: Into<Slice>>(
    batch: &mut OwnedWriteBatch,
    staged_bytes: &mut u64,
    partition: &Scoped,
    key: K,
    value: V,
) {
    let value = value.into();
    let value_len = value.len();
    let key_len = partition.stage_insert(batch, key, value);
    *staged_bytes += staged_entry_bytes(key_len, value_len);
}

/// Stage one delete. `WriteBatch::remove` stages a full `Item` carrying an empty value, so a
/// tombstone costs everything an insert does bar the value payload.
pub(super) fn stage_remove<K: Into<Vec<u8>>>(
    batch: &mut OwnedWriteBatch,
    staged_bytes: &mut u64,
    partition: &Scoped,
    key: K,
) {
    let key_len = partition.stage_remove(batch, key);
    *staged_bytes += staged_entry_bytes(key_len, 0);
}

impl IndexWriter {
    pub(super) fn new(db: IndexDb) -> Self {
        let batch = db.db.batch();
        Self {
            db,
            batch,
            staged_bytes: 0,
            writes_postings: false,
            changes_bm25_forward: false,
            #[cfg(feature = "code-search")]
            bm25_delta: super::bm25_pass::Bm25Delta::default(),
            #[cfg(feature = "code-search")]
            bm25_dirty_marked: false,
            #[cfg(feature = "code-search")]
            bm25_sweep_owed: false,
            content_sweep_owed: false,
            #[cfg(feature = "code-search")]
            bm25_in_flight: super::bm25_live::InFlight::default(),
            #[cfg(test)]
            fail_after_staging: false,
            #[cfg(all(test, feature = "code-search"))]
            fail_forward_read_after: None,
        }
    }

    /// Resident bytes staged into the current batch, per [`staged_entry_bytes`]. There is no reset:
    /// [`Self::commit`] consumes the writer, so the counter dies with the batch it describes and
    /// the next batch starts a fresh writer at zero.
    pub fn staged_bytes(&self) -> u64 {
        self.staged_bytes
    }

    /// Write the rows of `rel`, whose content has the [`keys::ContentKey`] `content`: its symbols
    /// and call sites (from `l1` and optionally `l2`) under `content`, shared by every view of the
    /// group holding it, and its path-keyed rows, replaced. The content's stored rows are reconciled
    /// with the fresh ones ([`super::writer_content`]): an identical extraction writes nothing, a
    /// different one replaces them whole. Calls are left as they are when `l2` is `None`. A view
    /// reads the content rows once its file map names `rel` under `content`
    /// ([`super::live_keys`]). Atomic.
    pub fn upsert_file(
        &mut self,
        rel: &RelPath,
        content: &keys::ContentKey,
        l1: &FileMapL1,
        l2: Option<&FileMapL2>,
    ) -> Result<(), IndexError> {
        // The content is pending, and its stored rows are read, under the group gate: a sweep then ~keep
        // either sees it pending or deleted the rows before this read (see `group_sweep`). ~keep
        let gate = std::sync::Arc::clone(&self.db.bm25_gate);
        let _shared = gate.read();
        if let Some(own) = &self.db.group {
            own.group.register_pending(own.view_id, *content);
        }
        self.owe_content_sweep();
        self.stage_deletes_for(rel)?;
        self.stage_inserts_for(rel, content, l1)?;
        if let Some(l2) = l2 {
            self.stage_call_inserts(rel, content, l2)?;
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

    /// Drop the path-keyed rows of `rel`. Used when a file is removed from the scan set. Its content
    /// rows stay for the views that hold the content; the group sweep collects them.
    pub fn remove_file(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        // The view is unsettled until its file map drops `rel` (see `group_sweep`). ~keep
        if let Some(own) = &self.db.group {
            own.group.register_path_write(own.view_id);
        }
        self.owe_content_sweep();
        self.stage_deletes_for(rel)
    }

    /// A path write may leave a content no view names once the file map follows: stage the group's
    /// owed content sweep into the same batch, once.
    fn owe_content_sweep(&mut self) {
        if self.content_sweep_owed || self.db.group.is_none() {
            return;
        }
        let token = super::group_sweep::fresh_token().to_be_bytes();
        let key = super::group_sweep::META_CONTENT_SWEEP_OWED;
        self.batch.insert(self.db.meta.raw(), key.to_vec(), token.to_vec());
        self.staged_bytes += staged_entry_bytes(key.len(), token.len());
        self.content_sweep_owed = true;
    }

    /// Drop every symbol and call row of `content`, for every view that holds it. Only for a
    /// content no view holds (the group sweep), or to model lost rows in a test.
    pub fn remove_content(&mut self, content: &keys::ContentKey) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        symbol_family(db).stage(batch, staged_bytes, content, Fresh::default())?;
        call_family(db).stage(batch, staged_bytes, content, Fresh::default())
    }

    /// Store the grep bloom `row` (built by [`super::grep_bloom::build_row`] from the bytes the
    /// scanner already holds) under the content hash `hash_hex`. Keyed by content alone, so the insert
    /// overwrites an identical row and no delete is staged; the row outlives any one path and is
    /// dropped by [`IndexDb::sweep_grep_bloom`]. Counted against the staged-byte budget like every
    /// other entry.
    pub fn upsert_grep_bloom(&mut self, hash_hex: &str, row: Vec<u8>) {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        stage_insert(batch, staged_bytes, &db.grep_bloom, hash_hex.as_bytes().to_vec(), row);
    }

    pub(super) fn remove_grep_bloom(&mut self, key: &[u8]) {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        stage_remove(batch, staged_bytes, &db.grep_bloom, key.to_vec());
    }

    /// Replace the resolved-reference edges whose *use* is in `use_rel` with those derived from
    /// `refs` (the file's resolution facts). Deletes the file's existing edges first (keyed by
    /// use file, O(prefix)), then inserts each intra-file edge into both `refs_by_def` (keyed by
    /// the defining site → `find_references`) and `refs_by_path` (keyed by the use site →
    /// `goto_definition`). Atomic within the batch. Cross-file edges are staged by the resolve
    /// pass separately once import resolution lands.
    ///
    /// An existing intra-file edge that `refs` keeps is left in place rather than deleted and
    /// inserted again: the index after the batch is the same, and a full rescan, which re-resolves
    /// every file, then writes only the edges that changed instead of every edge of the repository.
    pub fn upsert_resolved_file(&mut self, use_rel: &RelPath, refs: &FileResolvedRefs) -> Result<(), IndexError> {
        let mut fresh: ahash::AHashSet<(u32, u32)> =
            refs.intra.iter().map(|edge| (edge.use_start, edge.def_start)).collect();
        self.stage_resolved_deletes_keeping(use_rel, &mut fresh)?;
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        for edge in &refs.intra {
            if !fresh.remove(&(edge.use_start, edge.def_start)) {
                continue;
            }
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

    /// Atomically replace the file's forward rows and upsert its postings under `content`, the
    /// [`keys::Bm25Key`] of the file's content and chunk settings. The postings are shared by every
    /// view of the group holding the same key; the forward rows are this view's.
    #[cfg(feature = "code-search")]
    pub fn upsert_bm25_file(
        &mut self,
        rel: &RelPath,
        content: &keys::Bm25Key,
        postings: &[ChunkPosting],
    ) -> Result<(), IndexError> {
        // In flight before the holders are read, under the gate: a retirement then keeps the key's ~keep
        // postings until this batch commits its holder row (see `bm25_live::Bm25GateState`). ~keep
        let gate = std::sync::Arc::clone(&self.db.bm25_gate);
        let shared = gate.read();
        self.bm25_in_flight.register(&gate, *content);
        // Another path's holder row means the postings are stored; this path's own does not, so a ~keep
        // pass that writes a file again (a policy reconcile) restores whatever is missing. ~keep
        let own = keys::code_bm25_holder(content, self.db.holder_view_id(), rel);
        let stored = self
            .db
            .code_bm25_holders
            .prefix_keys(content)
            .take(2)
            .find(|holder| holder.as_ref().map_or(true, |holder| **holder != *own))
            .transpose()
            .map(|holder| holder.is_some());
        drop(shared);
        let staged = stored.and_then(|stored| {
            let old = self.stage_bm25_deletes_for(rel)?;
            // A holder row is found and removed only through a forward row: none without one. ~keep
            let holds = self.stage_bm25_inserts_for(rel, content, postings, !stored)?;
            self.stage_bm25_holders(rel, &old, Some(content), holds);
            Ok(())
        });
        #[cfg(test)]
        let staged = staged.and_then(|()| match self.fail_after_staging {
            true => Err(IndexError::Io {
                path: rel.to_path_buf(),
                source: std::io::Error::other("injected bm25 upsert failure after staging"),
            }),
            false => Ok(()),
        });
        // A file staged in part leaves rows the pass's tally may not describe. ~keep
        staged.inspect_err(|_| self.db.bm25_pass.poison())
    }

    /// Drop the file's forward rows. Its postings stay, unread by this view, until a retirement or
    /// the group's posting sweep finds no view naming their key.
    #[cfg(feature = "code-search")]
    pub fn remove_bm25_file(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        let old = self
            .stage_bm25_deletes_for(rel)
            .inspect_err(|_| self.db.bm25_pass.poison())?;
        self.stage_bm25_holders(rel, &old, None, false);
        Ok(())
    }

    /// Stage this view's holder rows of `rel`: drop those of the `old` keys its deleted forward rows
    /// named, then add the one of `new` when the file holds chunks under it. A key in both stays as
    /// it is, so no batch deletes and inserts one key.
    #[cfg(feature = "code-search")]
    fn stage_bm25_holders(&mut self, rel: &RelPath, old: &[keys::Bm25Key], new: Option<&keys::Bm25Key>, holds: bool) {
        let view_id = self.db.holder_view_id();
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let kept = new.filter(|_| holds);
        for key in old.iter().filter(|key| Some(*key) != kept) {
            stage_remove(
                batch,
                staged_bytes,
                &db.code_bm25_holders,
                keys::code_bm25_holder(key, view_id, rel),
            );
            self.writes_postings = true;
        }
        if let Some(key) = kept.filter(|key| !old.contains(key)) {
            stage_insert(
                batch,
                staged_bytes,
                &db.code_bm25_holders,
                keys::code_bm25_holder(key, view_id, rel),
                Vec::<u8>::new(),
            );
            self.writes_postings = true;
        }
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

    /// Record one importer's stitch outcome: each `(local_start, def_path, def_start)` binding into
    /// `import_targets`, and `probes` (every path the resolver consulted for the importer) into
    /// `importers_by_probe`, plus the manifest row [`Self::stage_import_probe_deletes_for`] later
    /// uses to remove them. Like [`Self::upsert_cross_file_edge`] it stages no delete: the importer's
    /// previous rows were purged by the `upsert_resolved_file` that preceded the stitch.
    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    pub fn upsert_import_probes(
        &mut self,
        importer: &RelPath,
        targets: &[(u32, RelPath, u32)],
        probes: &[keys::ProbeHash],
    ) {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        for (local_start, def_path, def_start) in targets {
            stage_insert(
                batch,
                staged_bytes,
                &db.import_targets,
                keys::import_target(importer, *local_start),
                keys::import_target_value(def_path, *def_start),
            );
        }
        for hash in probes {
            stage_insert(
                batch,
                staged_bytes,
                &db.importers_by_probe,
                keys::importer_by_probe(hash, importer),
                Vec::<u8>::new(),
            );
        }
        stage_insert(
            batch,
            staged_bytes,
            &db.import_targets,
            keys::import_target(importer, keys::PROBE_MANIFEST_START),
            keys::probe_manifest_value(probes),
        );
    }

    /// Flush this batch to disk atomically. Consumes the writer.
    ///
    /// `WriteBatch::commit` journals every staged item, then applies it to the memtables and runs
    /// fjall's own `check_memtable_rotate` + `local_backpressure` for each affected keyspace — so
    /// the commit cadence is also how often the writer *hears* fjall asking it to stall. Batching
    /// by bytes rather than by file count samples that signal an order of magnitude more often on
    /// entry-dense files, and bounds the journal write burst to the same budget.
    pub fn commit(self) -> Result<(), IndexError> {
        #[cfg(test)]
        if self.db.fault.take_injected_commit_failure() {
            #[cfg(feature = "code-search")]
            self.db.bm25_pass.poison();
            return Err(IndexError::Io {
                path: std::path::PathBuf::new(),
                source: std::io::Error::other("injected commit failure"),
            });
        }
        let gate = (self.writes_postings || self.content_sweep_owed).then(|| self.db.bm25_gate.read());
        let result = self.batch.commit().map_err(IndexError::from);
        drop(gate);
        if let Err(error) = &result {
            self.db.record_commit_error(error);
        }
        // A failed commit may still have reached the journal: the pass can no longer vouch. ~keep
        #[cfg(feature = "code-search")]
        if result.is_err() {
            self.db.bm25_pass.poison();
        } else if !self.bm25_delta.is_empty() && !self.db.bm25_pass.absorb(self.bm25_delta) {
            // Staged under a pass that has since closed: the view must not be vouched for. ~keep
            let token = super::bm25_pass::fresh_token().to_be_bytes();
            if let Err(error) = self.db.meta.insert(super::bm25_pass::META_BM25_DIRTY, token) {
                tracing::warn!(%error, "could not mark the BM25 stats dirty; the next pass may trust them");
            }
        }
        if self.changes_bm25_forward {
            // After the commit, failed or not: a cache built from rows read before it is stale. ~keep
            self.db.bm25_live.invalidate();
        }
        result
    }

    fn stage_deletes_for(&mut self, rel: &RelPath) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let impl_path_prefix = keys::impls_by_path_prefix(rel);
        let mut found_impls: Vec<(Vec<u8>, String, String, u32)> = Vec::new();
        for row in db.implementations_by_path.prefix(&impl_path_prefix) {
            let (k, _) = row?;
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
        self.stage_resolved_deletes_keeping(use_rel, &mut ahash::AHashSet::new())
    }

    /// [`Self::stage_resolved_deletes_for`], leaving in place each intra-file edge whose
    /// `(use_start, def_start)` is in `keep`. A kept edge is taken out of `keep`, so what remains
    /// there is what the caller still has to insert.
    fn stage_resolved_deletes_keeping(
        &mut self,
        use_rel: &RelPath,
        keep: &mut ahash::AHashSet<(u32, u32)>,
    ) -> Result<(), IndexError> {
        self.stage_import_probe_deletes_for(use_rel)?;
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let prefix = keys::refs_by_path_prefix(use_rel);
        let mut found: Vec<(Vec<u8>, RelPath, u32, u32)> = Vec::new();
        for row in db.refs_by_path.prefix(&prefix) {
            let (k, _) = row?;
            if let Some((_use_path, use_start, def_path, def_start)) = keys::parse_ref_by_path(&k) {
                if def_path == *use_rel && keep.remove(&(use_start, def_start)) {
                    continue;
                }
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

    /// Stage deletes for the `import_targets` rows of `importer` and the `importers_by_probe` rows
    /// its manifest names. An importer whose edges are re-resolved is always re-stitched afterwards
    /// (and so re-probed), and a removed importer must leave no rows behind.
    fn stage_import_probe_deletes_for(&mut self, importer: &RelPath) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let manifest_key = keys::import_target(importer, keys::PROBE_MANIFEST_START);
        for row in db.import_targets.prefix(&keys::import_targets_prefix(importer)) {
            let (k, v) = row?;
            if k.as_ref() == manifest_key.as_slice() {
                for hash in keys::parse_probe_manifest(&v) {
                    let probe_key = keys::importer_by_probe(&hash, importer);
                    stage_remove(batch, staged_bytes, &db.importers_by_probe, probe_key);
                }
            }
            stage_remove(batch, staged_bytes, &db.import_targets, (*k).to_vec());
        }
        Ok(())
    }

    #[cfg(feature = "code-search")]
    fn stage_bm25_deletes_for(&mut self, rel: &RelPath) -> Result<Vec<keys::Bm25Key>, IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let prefix = keys::code_bm25_by_path_prefix(rel);
        let keys: Vec<Vec<u8>> = if let Some(generation) = db.bm25_pass.open_generation() {
            // An open pass needs each row's value too: the postings it names and its doclen. The ~keep
            // tally is local until every row was read, so a read error leaves none of it behind. ~keep
            let mut keys = Vec::new();
            let mut delta = super::bm25_pass::Bm25Delta::default();
            for row in db.code_bm25_by_path.prefix(&prefix) {
                #[cfg(test)]
                if self.fail_forward_read_after == Some(keys.len()) {
                    return Err(IndexError::Io {
                        path: rel.to_path_buf(),
                        source: std::io::Error::other("injected forward-row read failure"),
                    });
                }
                let (key, value) = row?;
                delta.remove_row(&db.bm25_pass, &key, &value);
                keys.push(key.to_vec());
            }
            self.bm25_delta.staged_under(generation);
            self.bm25_delta.merge(delta);
            keys
        } else {
            db.code_bm25_by_path
                .prefix_keys(&prefix)
                .map(|key| key.map(|key| key.to_vec()))
                .collect::<Result<_, _>>()?
        };
        let deleted = !keys.is_empty();
        self.changes_bm25_forward |= deleted;
        let mut old: Vec<keys::Bm25Key> = Vec::new();
        for key in keys {
            if let Some((_, content, _)) = keys::parse_code_bm25_by_path(&key)
                && !old.contains(&content)
            {
                old.push(content);
            }
            stage_remove(batch, staged_bytes, &db.code_bm25_by_path, key);
        }
        if self.changes_bm25_forward {
            self.mark_bm25_dirty_outside_pass();
        }
        if deleted {
            self.owe_bm25_sweep_outside_pass();
        }
        Ok(old)
    }

    /// A forward-row delete outside an open pass leaves its postings for no retirement: stage the
    /// group's owed sweep into the same batch, once, and hold the posting gate for the commit so a
    /// sweep that is clearing the debt cannot miss it.
    #[cfg(feature = "code-search")]
    fn owe_bm25_sweep_outside_pass(&mut self) {
        if self.bm25_sweep_owed || self.db.bm25_pass.is_open() {
            return;
        }
        let token = super::bm25_pass::fresh_token().to_be_bytes();
        let key = super::bm25_pass::META_BM25_SWEEP_OWED;
        self.batch.insert(self.db.meta.raw(), key.to_vec(), token.to_vec());
        self.staged_bytes += staged_entry_bytes(key.len(), token.len());
        self.writes_postings = true;
        self.bm25_sweep_owed = true;
    }

    /// A forward-row write outside an open pass makes the view's stored BM25 stats unvouched for:
    /// stage a fresh dirty token into the same batch, once.
    #[cfg(feature = "code-search")]
    fn mark_bm25_dirty_outside_pass(&mut self) {
        if self.bm25_dirty_marked || self.db.bm25_pass.is_open() {
            return;
        }
        let token = super::bm25_pass::fresh_token().to_be_bytes();
        stage_insert(
            &mut self.batch,
            &mut self.staged_bytes,
            &self.db.meta,
            super::bm25_pass::META_BM25_DIRTY.to_vec(),
            token.to_vec(),
        );
        self.bm25_dirty_marked = true;
    }

    #[cfg(feature = "code-search")]
    pub(super) fn remove_bm25_posting(&mut self, key: &[u8]) {
        stage_remove(
            &mut self.batch,
            &mut self.staged_bytes,
            &self.db.code_bm25_postings,
            key.to_vec(),
        );
    }

    /// Stage inserts for `rel`'s BM25 postings: one shared `code_bm25_postings` row per term of the
    /// content packing `ordinal ‖ tf ‖ doclen` of every chunk holding it (only when `write_postings`),
    /// plus one `code_bm25_by_path` forward entry per chunk carrying `doclen ‖ content hash ‖ terms`.
    /// A posting whose chunk id does not parse is skipped. Returns whether a forward entry was
    /// staged: only then does the file hold `content`.
    #[cfg(feature = "code-search")]
    fn stage_bm25_inserts_for(
        &mut self,
        rel: &RelPath,
        content: &keys::Bm25Key,
        postings: &[ChunkPosting],
        write_postings: bool,
    ) -> Result<bool, IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let mut holds = false;
        let mut by_term: ahash::AHashMap<&str, Vec<(u32, u32, u32)>> = ahash::AHashMap::new();
        for posting in postings {
            let Some((hash, ordinal)) = keys::split_chunk_id(&posting.chunk_id) else {
                tracing::debug!(path = %rel, chunk = %posting.chunk_id, "index: unparsable chunk id; posting skipped");
                continue;
            };
            // Another holder wrote this key's postings: they are a function of the key. ~keep
            for (term, tf) in posting.terms.iter().filter(|_| write_postings) {
                by_term
                    .entry(term.as_str())
                    .or_default()
                    .push((ordinal, *tf, posting.doclen));
            }
            let mut term_names: Vec<&str> = posting.terms.iter().map(|(t, _)| t.as_str()).collect();
            term_names.sort_unstable();
            let terms = rmp_serde::to_vec(&term_names)?;
            let Some(value) = keys::code_bm25_by_path_value(posting.doclen, hash, &terms) else {
                continue;
            };
            stage_insert(
                batch,
                staged_bytes,
                &db.code_bm25_by_path,
                keys::code_bm25_by_path(rel, content, ordinal),
                value,
            );
            if let Some(generation) = db.bm25_pass.open_generation() {
                self.bm25_delta.staged_under(generation);
                self.bm25_delta.add_row(posting.doclen);
            }
            holds = true;
            self.writes_postings = true;
            self.changes_bm25_forward = true;
        }
        for (term, mut entries) in by_term {
            entries.sort_unstable();
            if let Some(posting_key) = keys::code_bm25_posting(term, content) {
                stage_insert(
                    batch,
                    staged_bytes,
                    &db.code_bm25_postings,
                    posting_key,
                    keys::code_bm25_posting_value(&entries),
                );
                self.writes_postings = true;
            }
        }
        if self.changes_bm25_forward {
            self.mark_bm25_dirty_outside_pass();
        }
        Ok(holds)
    }

    /// Stage the L2 call sites of `content` into both call partitions.
    fn stage_call_inserts(
        &mut self,
        rel: &RelPath,
        content: &keys::ContentKey,
        l2: &FileMapL2,
    ) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        // Before the keys are staged: the dictionary must never miss a name that has a key.
        db.callee_names.record(l2.calls.iter().map(|call| call.callee.as_str()));
        let mut fresh = Fresh {
            rows: Vec::with_capacity(l2.calls.len()),
            names: Vec::new(),
        };
        let mut starts_by_callee: ahash::AHashMap<&str, Vec<u32>> = ahash::AHashMap::new();
        for call in &l2.calls {
            starts_by_callee
                .entry(call.callee.as_str())
                .or_default()
                .push(call.start_byte);
            fresh.rows.push(FreshRow {
                row: keys::content_row(content, call.start_byte),
                value: rmp_serde::to_vec_named(call)?,
            });
        }
        for (callee, starts) in starts_by_callee {
            match keys::call_name_row(callee, content) {
                Some(name_key) => fresh.names.push((name_key, keys::pack_starts(starts))),
                None => tracing::debug!(
                    path = %rel,
                    callee_len = callee.len(),
                    "index: callee name exceeds 64 KiB — skipping calls_by_callee entry"
                ),
            }
        }
        call_family(db).stage(batch, staged_bytes, content, fresh)
    }

    fn stage_inserts_for(
        &mut self,
        rel: &RelPath,
        content: &keys::ContentKey,
        l1: &FileMapL1,
    ) -> Result<(), IndexError> {
        let (db, batch, staged_bytes) = (&self.db, &mut self.batch, &mut self.staged_bytes);
        let mut fresh = Fresh {
            rows: Vec::with_capacity(l1.symbols.len()),
            names: Vec::with_capacity(l1.symbols.len()),
        };
        for sym in &l1.symbols {
            match keys::symbol_by_name(&sym.name, sym.kind, content, sym.start_byte) {
                Some(name_key) => fresh.names.push((name_key, Vec::new())),
                None => tracing::debug!(
                    path = %rel,
                    name_len = sym.name.len(),
                    "index: symbol name exceeds 64 KiB — skipping symbols_by_name entry"
                ),
            }
            fresh.rows.push(FreshRow {
                row: keys::content_row(content, sym.start_byte),
                value: rmp_serde::to_vec_named(sym)?,
            });
        }
        symbol_family(db).stage(batch, staged_bytes, content, fresh)?;
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
