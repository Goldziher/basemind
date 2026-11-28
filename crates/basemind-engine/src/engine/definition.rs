//! Following a use to its definition through the index, for `code_refs` relation `definition`.

use crate::path::RelPath;

/// `resolution_note` when a cross-file edge was read from an index whose resolve pass is unsettled,
/// or that holds other content for one of the files than this session's file map.
pub const SNAPSHOT_REFUSED: &str = "the index is updating for the files involved; retry in a moment";

/// The definition the use at `pos` in `use_path` binds to. `intra_def` is the in-file binding the
/// file's resolution facts name, which the index hop starts from and which is served when the hop
/// finds nothing or is refused. A store that borrows its index ([`Store::index_source`], a
/// daemon-hosted session) has each hop checked ([`follow`]).
///
/// [`Store::index_source`]: crate::store::Store
pub(crate) fn definition_of_use(
    store: &crate::store::Store,
    use_path: &RelPath,
    pos: u32,
    intra_def: Option<u32>,
) -> Hop {
    let hosted = store.index_source.is_some();
    match intra_def {
        Some(def_start) => {
            let hop = follow(store, hosted, use_path, def_start);
            Hop {
                found: Some(hop.found.unwrap_or((use_path.clone(), def_start))),
                refused: hop.refused,
            }
        }
        None => follow(store, hosted, use_path, pos),
    }
}

/// What following a use through the index found: the definition, or that one was found and refused
/// because it may not belong to the content this session reads.
#[derive(Default)]
pub struct Hop {
    pub(crate) found: Option<(RelPath, u32)>,
    pub(crate) refused: bool,
}

/// The definition the use at `(use_path, use_start)` binds to.
///
/// A daemon-hosted session (`hosted`) reads the pool writer's index, which runs ahead of its file
/// map from a scan's flush until the session refreshes, and whose cross-file edges are of the
/// files' previous contents while a resolve pass runs or after one was cancelled or aborted. Its
/// edge is kept only when the pass was settled before and after the read, the writer published no
/// file map in between, and the writer's content keys for the use file and the landing file are
/// the session's. Any other session reads an index no scan moves while its store guard is held.
fn follow(store: &crate::store::Store, hosted: bool, use_path: &RelPath, use_start: u32) -> Hop {
    use crate::index::view::IndexView as _;

    let index = hosted.then(|| store.index_handle()).flatten();
    let Some(index) = index else {
        return Hop {
            found: crate::query::definition_of(store, use_path, use_start),
            refused: false,
        };
    };
    let (live, settled) = (index.live_keys(), resolve_pass_complete(store));
    let Some((path, start)) = index.view().definition_of(use_path, use_start) else {
        return Hop::default();
    };
    let current = index.live_keys();
    let session_key = |rel: &RelPath| store.lookup(rel).and_then(|entry| entry.content_key);
    let in_snapshot = settled
        && resolve_pass_complete(store)
        && std::sync::Arc::ptr_eq(&live, &current)
        && same_content(session_key(use_path), current.key_of(use_path))
        && same_content(session_key(&path), current.key_of(&path));
    if in_snapshot {
        Hop {
            found: Some((path, start)),
            refused: false,
        }
    } else {
        tracing::debug!(%use_path, %path, settled, "goto_definition: the writer's edge is not of this session's snapshot; refused");
        Hop {
            found: None,
            refused: true,
        }
    }
}

/// Whether the session and the writer both hold a file, with the same content.
fn same_content(
    session: Option<crate::index::keys::ContentKey>,
    writer: Option<&crate::index::keys::ContentKey>,
) -> bool {
    session.is_some_and(|session| writer == Some(&session))
}

/// Whether the last scan's resolve pass committed. The scan stamps `status.json` as resolving when
/// it flushes the code map and as complete once the pass has committed every edge; a cancelled or
/// aborted scan leaves it partial. A missing sidecar vouches for nothing either way and reads as
/// complete, as it does for the boot-time rescan decision.
pub(crate) fn resolve_pass_complete(store: &crate::store::Store) -> bool {
    crate::store::read_status_sidecar(&store.basemind_dir).is_none_or(|status| !status.is_degraded())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edge_is_of_the_snapshot_only_when_both_sides_hold_one_content() {
        let (old, new) = ([1u8; 16], [2u8; 16]);
        assert!(same_content(Some(old), Some(&old)));
        assert!(
            !same_content(Some(old), Some(&new)),
            "the writer rescanned the file first"
        );
        assert!(!same_content(Some(old), None), "the writer no longer holds the file");
        assert!(!same_content(None, Some(&old)), "the session does not hold it yet");
        assert!(!same_content(None, None));
    }
}
