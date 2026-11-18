//! `code_refs` call sites listed from the index, laid against the file as it is now: a hit in a file
//! edited since the scan is flagged `stale` (see `engine/stale.rs`, which `code_symbols` uses too).

use ahash::AHashMap;

use super::ServerState;
use super::types::ReferenceHit;
use crate::engine::stale::{Disk, disk};
use crate::path::RelPath;

/// Flag each call site listed in a file edited since the scan. Its line is the indexed one: a call
/// site has no name of its own to find it again by. Blocking IO.
pub(super) fn flag_call_sites(state: &ServerState, cache: &super::MapCache, hits: &mut [ReferenceHit]) {
    let mut judged: AHashMap<RelPath, bool> = AHashMap::new();
    for hit in hits {
        hit.stale = *judged
            .entry(hit.path.clone())
            .or_insert_with(|| !matches!(disk(&state.shared, cache, &hit.path), Disk::Fresh));
    }
}
