//! The in-RAM mirror of the `calls_by_callee` and `calls_by_path` keyspaces, projected from the L2
//! call blobs for a session that can reach no Fjall index. The name scan over it lives with the
//! Fjall one in `src/mcp/helpers_calls_scan.rs`.

/// How many files one projection pass decodes before folding them into the views. Bounds the
/// decoded-L2 working set to O(chunk) rather than O(corpus).
const L2_PROJECTION_CHUNK: usize = 256;

/// In-RAM mirror of the Fjall `calls_by_callee` + `calls_by_path` keyspaces, built
/// from the L2 call blobs for read-only `serve` sessions that can't open the
/// single-holder Fjall index. Lets unlimited concurrent sessions answer
/// `find_references` / `find_callers` from the shared, immutable,
/// concurrently-readable blobs.
pub(crate) struct InRamCallIndex {
    /// Sorted ascending by `key` to match Fjall's `range` iteration order
    /// (drives `find_references` / `find_callers`).
    entries: Vec<InRamCall>,
    /// path → its call sites (the `calls_by_path` keyspace).
    by_path: ahash::AHashMap<crate::path::RelPath, Vec<CallRef>>,
    /// True when the byte budget cut the projection short, so its answers are incomplete.
    capped: bool,
}

/// One call site of the projection, under the key a name scan orders it by.
pub(crate) struct InRamCall {
    /// `keys::call_by_callee_hit(callee, content, start_byte, rel)` — the exact key the index view
    /// yields, reused so cursors round-trip identically across the two paths.
    pub(crate) key: Vec<u8>,
    /// The callee identifier.
    pub(crate) callee: String,
    /// The file holding the call.
    pub(crate) rel: crate::path::RelPath,
    /// 0-based byte offset of the call site (for containing-function resolution).
    pub(crate) start_byte: u32,
    /// 1-based line (`start_row + 1`), matching `resolve_call_line_col`.
    pub(crate) line: u32,
    /// 0-based byte column.
    pub(crate) column: u32,
}

/// A call site within a file: the callee identifier, its start byte offset, and its position.
#[derive(Clone)]
pub(crate) struct CallRef {
    pub callee: String,
    pub start_byte: u32,
    /// 1-based line (`start_row + 1`), matching `resolve_call_line_col`.
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
}

impl InRamCallIndex {
    /// Project the L2 calls of every file into the two views, bounded by `budget_bytes`
    /// (`0` = unbounded).
    ///
    /// Reads/decodes run in parallel a chunk at a time; the two views are assembled serially per
    /// chunk. See [`capped`](Self::capped) for what happens when the budget is reached.
    pub(crate) fn build(
        files: &super::l1_cache::FileIndexView,
        blobs_dir: &std::path::Path,
        budget_bytes: u64,
    ) -> Self {
        use rayon::prelude::*;
        let paths: Vec<(&crate::path::RelPath, &str, crate::index::keys::ContentKey)> = files
            .iter()
            .map(|(rel, meta)| (rel, &*meta.hash_hex, meta.order_key()))
            .collect();
        let mut entries: Vec<InRamCall> = Vec::new();
        let mut by_path: ahash::AHashMap<crate::path::RelPath, Vec<CallRef>> =
            ahash::AHashMap::with_capacity(paths.len());
        let mut charged: u64 = 0;
        let mut capped = false;
        // Chunked rather than one whole-corpus `par_iter`: the previous shape materialised EVERY
        // file's decoded L2 calls before assembling either view, so peak residency was the sum of
        // the projection and the raw decode it was projected from. Here at most one chunk of
        // decoded blobs is live, and the loop stops the moment the projection reaches its budget.
        for chunk in paths.chunks(L2_PROJECTION_CHUNK) {
            let per_file: Vec<(
                crate::path::RelPath,
                crate::index::keys::ContentKey,
                Vec<crate::extract::Call>,
            )> = chunk
                .par_iter()
                .filter_map(|(rel, hash_hex, content)| {
                    let calls = crate::store_blob::read_l2_blob_in(blobs_dir, hash_hex)
                        .ok()
                        .flatten()?
                        .calls;
                    Some(((*rel).clone(), *content, calls))
                })
                .collect();
            for (rel, content, calls) in per_file {
                let mut refs: Vec<CallRef> = Vec::with_capacity(calls.len());
                for call in calls {
                    charged += (call.callee.len() + rel.as_bytes().len()) as u64
                        + (std::mem::size_of::<InRamCall>() + std::mem::size_of::<CallRef>()) as u64;
                    if budget_bytes != 0 && charged > budget_bytes {
                        capped = true;
                        break;
                    }
                    if let Some(key) =
                        crate::index::keys::call_by_callee_hit(&call.callee, &content, call.start_byte, &rel)
                    {
                        entries.push(InRamCall {
                            key,
                            callee: call.callee.clone(),
                            rel: rel.clone(),
                            start_byte: call.start_byte,
                            line: call.start_row + 1,
                            column: call.start_col,
                        });
                    }
                    refs.push(CallRef {
                        line: call.start_row + 1,
                        column: call.start_col,
                        callee: call.callee,
                        start_byte: call.start_byte,
                    });
                }
                by_path.insert(rel, refs);
                if capped {
                    break;
                }
            }
            if capped {
                break;
            }
        }
        entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Self {
            entries,
            by_path,
            capped,
        }
    }

    /// A projection of no files.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            entries: Vec::new(),
            by_path: ahash::AHashMap::new(),
            capped: false,
        }
    }

    /// Every call site, sorted by key: the order a name scan walks.
    pub(crate) fn entries(&self) -> &[InRamCall] {
        &self.entries
    }

    /// Whether the byte budget truncated this projection.
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    /// All call sites in `rel` (the `calls_by_path` keyspace).
    pub(crate) fn calls_in_file(&self, rel: &crate::path::RelPath) -> &[CallRef] {
        self.by_path.get(rel).map_or(&[], Vec::as_slice)
    }
}
