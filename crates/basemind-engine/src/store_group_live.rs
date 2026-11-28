//! Which contents a repository group still holds, for the sweeps of content-keyed rows.
//!
//! The grep bloom and the BM25 postings are keyed by content, not by path. In a group index
//! (ADR-0028) one such row serves every worktree of the group holding that content, so a sweep run
//! by one worktree may drop a row only when no view of the group holds the content: its own
//! in-memory file map, and what the file map of every other view names. Each sibling's map is
//! decoded once and its holds kept, until that view writes its map again, so a sweep after a
//! watcher batch decodes none of the siblings that did not write. A file map that cannot be read
//! and is not provably of another schema makes the liveness unknowable, and the sweep is skipped.
//!
//! Symbol and call rows are keyed by content too, but read rather than swept here: a view reads them
//! through the live keys its file map names (`live_keys_of`, `Store::publish_live_keys`).
//!
//! The document rows of the retrieval store (ADR-0029) are keyed the same way, by [`doc_key`]:
//! [`Store::group_live_doc_keys`] lists the keys every view of the group names.

use std::path::{Path, PathBuf};

use ahash::AHashSet;

use crate::index::keys::ContentKey;
use crate::index::live_keys::LiveKeys;
use crate::store::{Index, Store, StoreError, ViewStamp};
use crate::store_hash_refs::HashRefs;

/// Why the group's liveness could not be established. A sweep given this must delete nothing.
#[derive(Debug, thiserror::Error)]
pub enum LivenessUnknown {
    /// A view's file map could not be read, so the contents it holds are unknown.
    #[error("file map at {path} is unreadable, so the contents it holds are unknown: {source}")]
    UnreadableMap {
        path: PathBuf,
        #[source]
        source: StoreError,
    },
    /// The store has no handle on its group index, so the group's other views cannot be listed.
    #[error("the store at {view_dir} holds no group index handle, so the group's other views are unknown")]
    NoGroup { view_dir: PathBuf },
}

/// The key of one document's rows in the retrieval store (ADR-0028 rule 2, ADR-0029): 16 bytes of
/// blake3 over the blob schema, the content hash and the extraction digest the scan recorded, as
/// hex. The same bytes extracted under the same settings by the same extractor are one set of rows
/// wherever and however often they appear; a different setting or a new extractor makes a new set,
/// so no row of an older extraction is served for a newer one.
pub fn doc_key(content_hash_hex: &str, doc_digest: &str) -> String {
    let mut input = Vec::with_capacity(content_hash_hex.len() + doc_digest.len() + 32);
    input.extend_from_slice(b"basemind/doc_key/1\0");
    input.extend_from_slice(&crate::extract::SCHEMA_VER.to_le_bytes());
    for part in [content_hash_hex, doc_digest] {
        input.extend_from_slice(&u32::try_from(part.len()).unwrap_or(u32::MAX).to_le_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    let hash = crate::hashing::hash_bytes(&input);
    crate::hashing::hex(&hash)[..32].to_string()
}

/// Every document key `index` names.
fn doc_keys_of(index: &Index) -> impl Iterator<Item = String> + '_ {
    index
        .doc_files
        .values()
        .map(|entry| doc_key(&entry.hash_hex, &entry.config_digest))
}

/// What one sibling view's file map names, kept so a sweep reads each map once and again only when
/// the view has written since. Contents are held by 8-byte digests ([`HashRefs`]): a collision can
/// only keep a row, never drop one.
#[derive(Debug, Default)]
struct ViewHolds {
    contents: HashRefs,
    doc_keys: AHashSet<String>,
    content_keys: AHashSet<ContentKey>,
}

impl ViewHolds {
    fn of(index: &Index) -> Self {
        let mut contents = HashRefs::default();
        for hash in index
            .files
            .values()
            .map(|entry| entry.hash_hex.as_str())
            .chain(index.rebuild_pins.iter().map(String::as_str))
        {
            contents.acquire(hash);
        }
        Self {
            contents,
            doc_keys: doc_keys_of(index).collect(),
            content_keys: index.files.values().filter_map(|entry| entry.content_key).collect(),
        }
    }
}

/// The most views the process-wide cache keeps before it starts over: a bound, not a working set.
const HOLDS_CACHE_CAP: usize = 256;

type HoldsCache = std::sync::Mutex<ahash::AHashMap<PathBuf, (ViewStamp, std::sync::Arc<ViewHolds>)>>;

fn holds_cache() -> &'static HoldsCache {
    static CACHE: std::sync::OnceLock<HoldsCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// What the file map under `view_dir` names. A view with no file map holds nothing; one of an older
/// or newer schema, or of an earlier blob layout, is rebuilt by its next scan before it is read
/// again, so its rows are not a sweep's to keep.
///
/// Served from the cache while the view's files are as they were when last decoded ([`ViewStamp`]);
/// otherwise decoded again. A map that cannot be read is never cached.
fn holds_of(view_dir: &Path) -> Result<std::sync::Arc<ViewHolds>, LivenessUnknown> {
    // Taken before the read: a write during it leaves a stamp that no longer matches, so the next
    // sweep decodes again. ~keep
    let stamp = ViewStamp::of(view_dir);
    let cached = holds_cache().lock().ok().and_then(|cache| {
        cache
            .get(view_dir)
            .filter(|(known, _)| *known == stamp)
            .map(|(_, holds)| holds.clone())
    });
    if let Some(holds) = cached {
        return Ok(holds);
    }
    let holds = match crate::store::read_index(view_dir) {
        Ok(Some(index)) => ViewHolds::of(&index),
        Ok(None)
        | Err(
            StoreError::SchemaMismatch { .. } | StoreError::NewerSchema { .. } | StoreError::StaleBlobLayout { .. },
        ) => ViewHolds::default(),
        Err(source) => {
            return Err(LivenessUnknown::UnreadableMap {
                path: view_dir.to_path_buf(),
                source,
            });
        }
    };
    let holds = std::sync::Arc::new(holds);
    if let Ok(mut cache) = holds_cache().lock() {
        if cache.len() >= HOLDS_CACHE_CAP {
            cache.clear();
        }
        cache.insert(view_dir.to_path_buf(), (stamp, holds.clone()));
    }
    Ok(holds)
}

/// The holds of every view under `view_dirs` (see [`holds_of`]).
fn holds_of_all(
    view_dirs: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<std::sync::Arc<ViewHolds>>, LivenessUnknown> {
    view_dirs.into_iter().map(|view_dir| holds_of(&view_dir)).collect()
}

/// The contents held by the views of a group other than the sweeping one, read from their file maps.
#[derive(Debug, Default)]
pub struct SiblingContents(Vec<std::sync::Arc<ViewHolds>>);

impl SiblingContents {
    /// The contents named by the file maps under `view_dirs` (see [`holds_of`]).
    pub(crate) fn read(view_dirs: impl IntoIterator<Item = PathBuf>) -> Result<Self, LivenessUnknown> {
        holds_of_all(view_dirs).map(Self)
    }

    fn holds(&self, hash_hex: &str) -> bool {
        self.0.iter().any(|view| view.contents.is_held(hash_hex))
    }
}

/// Liveness of a content for one sweep: held by the sweeping store's own file map or by a sibling.
pub struct ContentLiveness<'a> {
    store: &'a Store,
    siblings: SiblingContents,
}

impl ContentLiveness<'_> {
    /// Whether any view of the group holds `hash_hex`.
    pub(crate) fn holds(&self, hash_hex: &str) -> bool {
        self.store.hash_is_held(hash_hex) || self.siblings.holds(hash_hex)
    }

    /// Whether a view of the group other than the sweeping one holds `hash_hex`.
    #[cfg(feature = "code-search")]
    pub(crate) fn siblings_hold(&self, hash_hex: &str) -> bool {
        self.siblings.holds(hash_hex)
    }
}

impl Store {
    /// The liveness a sweep of this store's content-keyed rows must respect. Read at the time of the
    /// sweep: a sibling's file map changes with every scan of it.
    pub(crate) fn content_liveness(&self) -> Result<ContentLiveness<'_>, LivenessUnknown> {
        Ok(ContentLiveness {
            store: self,
            siblings: SiblingContents::read(self.sibling_view_dirs()?)?,
        })
    }

    /// [`Self::content_liveness`] for a sweep of rows every view of the group shares. Fails when the
    /// store holds no group index handle: its sibling views are unknown, not absent.
    #[cfg(feature = "code-search")]
    pub(crate) fn group_content_liveness(&self) -> Result<ContentLiveness<'_>, LivenessUnknown> {
        self.require_group()?;
        self.content_liveness()
    }

    /// `Ok` while the store holds a handle on its group index, through which the group's other views
    /// are listed.
    fn require_group(&self) -> Result<(), LivenessUnknown> {
        if self.index_db.as_ref().is_none_or(|db| db.group.is_none()) {
            return Err(LivenessUnknown::NoGroup {
                view_dir: self.view_dir.clone(),
            });
        }
        Ok(())
    }

    /// The distinct content hashes of this view's files, sorted, while it holds at most `cap` files:
    /// the contents its code search may answer from. The rows of the group's code-chunk store are
    /// keyed by them. `None` for a larger view, whose list is not built.
    #[cfg(feature = "code-search")]
    pub(crate) fn live_content_hashes(&self, cap: usize) -> Option<Vec<String>> {
        if self.index.files.len() > cap {
            return None;
        }
        let mut hashes: Vec<String> = self.index.files.values().map(|entry| entry.hash_hex.clone()).collect();
        hashes.sort_unstable();
        hashes.dedup();
        Some(hashes)
    }

    /// The view directories of every other view sharing this store's index database: none for a
    /// database of its own, since only a group index shares rows across views.
    pub(crate) fn sibling_view_dirs(&self) -> Result<Vec<PathBuf>, LivenessUnknown> {
        let Some(db) = self.index_db.as_ref() else {
            return Ok(Vec::new());
        };
        let views = db.sibling_views().map_err(|error| LivenessUnknown::UnreadableMap {
            path: self.view_dir.clone(),
            source: error.into(),
        })?;
        let workspaces = self.data_root.workspaces_dir();
        Ok(views
            .into_iter()
            .map(|(workspace, view)| workspaces.join(workspace).join(crate::store::VIEWS_DIR).join(view))
            .collect())
    }

    /// The document key of every document named by any view of this store's group: this store's own
    /// file map in memory and every other view's file map on disk, found through the group's view
    /// records. A retrieval-store row under any other key belongs to no view and may be swept.
    ///
    /// Keys are computed from what each file map records (content hash and extraction digest), never
    /// from the current configuration. Fails, and a sweep must then delete nothing, when the store
    /// holds no group index handle or a sibling's file map cannot be read.
    pub fn group_live_doc_keys(&self) -> Result<AHashSet<String>, LivenessUnknown> {
        self.require_group()?;
        let mut keys = self.sibling_doc_keys()?;
        keys.extend(doc_keys_of(&self.index));
        Ok(keys)
    }

    /// The document keys the file maps of the group's other views name, read from disk (see
    /// [`for_each_map`]). Fails, and a sweep must then delete nothing, when one cannot be read.
    pub(crate) fn sibling_doc_keys(&self) -> Result<AHashSet<String>, LivenessUnknown> {
        sibling_doc_keys_in(self.sibling_view_dirs()?)
    }

    /// The content key of every file named by any view of this store's group: this store's own file
    /// map in memory and every other view's file map on disk (see [`for_each_map`]), each entry's
    /// recorded key. Fails, and the group sweep must then delete no content, when a sibling's file
    /// map cannot be read.
    pub(crate) fn group_live_content_keys(&self) -> Result<AHashSet<ContentKey>, LivenessUnknown> {
        let mut keys: AHashSet<ContentKey> = self
            .index
            .files
            .values()
            .filter_map(|entry| entry.content_key)
            .collect();
        for view in holds_of_all(self.sibling_view_dirs()?)? {
            keys.extend(view.content_keys.iter().copied());
        }
        Ok(keys)
    }

    /// Publish the live keys of the file map just written, so the view's reads follow it.
    pub(crate) fn publish_live_keys(&self) {
        if let Some(db) = self.index_db.as_ref() {
            db.publish_live_keys(live_keys_of(&self.index));
        }
    }

    /// Publish the live keys of the file map this open loaded, unless a writer of this process
    /// already published the view's.
    pub(crate) fn publish_live_keys_if_unset(&self) {
        if let Some(db) = self.index_db.as_ref() {
            db.publish_live_keys_if_unset(|| live_keys_of(&self.index));
        }
    }

    /// Publish the live keys of the file map on disk, unless the view's are already published: for
    /// a reopened handle, whose store may hold entries not yet flushed. Those name rows a reader must
    /// not see before the map that lists them is written. A map that cannot be read publishes none.
    pub(crate) fn publish_written_live_keys_if_unset(&self) {
        if let Some(db) = self.index_db.as_ref() {
            db.publish_live_keys_if_unset(|| match crate::store::read_index(&self.view_dir) {
                Ok(Some(written)) => live_keys_of(&written),
                _ => LiveKeys::default(),
            });
        }
    }
}

/// The content keys a file map names, each with its paths (ADR-0028 rule 7): the key each entry
/// records its rows were written under (rule 3), never one recomputed. An entry that records none
/// names no rows.
pub(crate) fn live_keys_of(index: &Index) -> LiveKeys {
    LiveKeys::from_entries(
        index
            .files
            .iter()
            .filter_map(|(rel, entry)| Some((rel.clone(), entry.content_key?))),
    )
}

/// The document keys named by the file maps under `view_dirs`.
fn sibling_doc_keys_in(view_dirs: impl IntoIterator<Item = PathBuf>) -> Result<AHashSet<String>, LivenessUnknown> {
    let mut keys = AHashSet::new();
    for view in holds_of_all(view_dirs)? {
        keys.extend(view.doc_keys.iter().cloned());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::store::{DocEntry, FileEntry, INDEX_FILE, Index};

    /// The document keys named by `own` and by the file maps under `view_dirs`, as
    /// [`Store::group_live_doc_keys`] gathers them.
    fn live_doc_keys_in(
        own: &Index,
        view_dirs: impl IntoIterator<Item = PathBuf>,
    ) -> Result<AHashSet<String>, LivenessUnknown> {
        let mut keys = sibling_doc_keys_in(view_dirs)?;
        keys.extend(doc_keys_of(own));
        Ok(keys)
    }

    fn doc_entry(hash: &str, digest: &str) -> DocEntry {
        DocEntry {
            hash_hex: hash.to_string(),
            embedding_preset: String::new(),
            size_bytes: 1,
            mtime: 0,
            embedded: false,
            embed_attempted: false,
            config_digest: digest.to_string(),
        }
    }

    /// A file map holding the documents `docs`, as `(content hash, extraction digest)`.
    fn doc_map(docs: &[(&str, &str)]) -> Index {
        let mut index = Index::empty();
        for (i, (hash, digest)) in docs.iter().enumerate() {
            index.doc_files.insert(
                crate::path::RelPath::from(format!("d{i}.pdf").as_str()),
                doc_entry(hash, digest),
            );
        }
        index
    }

    fn write_index(view_dir: &Path, index: &Index) {
        std::fs::create_dir_all(view_dir).expect("view dir");
        std::fs::write(
            view_dir.join(INDEX_FILE),
            rmp_serde::to_vec_named(index).expect("encode"),
        )
        .expect("write");
    }

    fn write_map(view_dir: &Path, hashes: &[&str]) {
        std::fs::create_dir_all(view_dir).expect("view dir");
        let mut index = Index::empty();
        for (i, hash) in hashes.iter().enumerate() {
            index.files.insert(
                crate::path::RelPath::from(format!("f{i}.rs").as_str()),
                FileEntry {
                    hash_hex: (*hash).to_string(),
                    language: "rust".into(),
                    size_bytes: 1,
                    mtime: 0,
                    extract_epoch: 0,
                    content_key: None,
                    lines: 0,
                },
            );
        }
        std::fs::write(
            view_dir.join(INDEX_FILE),
            rmp_serde::to_vec_named(&index).expect("encode"),
        )
        .expect("write");
    }

    #[test]
    fn contents_of_every_sibling_file_map_are_held() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (b, c, missing) = (temp.path().join("b"), temp.path().join("c"), temp.path().join("none"));
        write_map(&b, &["only-in-b"]);
        write_map(&c, &["only-in-c", "shared"]);

        let siblings = SiblingContents::read([b, c, missing]).expect("readable");

        assert!(siblings.holds("only-in-b") && siblings.holds("only-in-c") && siblings.holds("shared"));
        assert!(!siblings.holds("in-no-map"));
    }

    #[test]
    fn a_sibling_file_map_is_decoded_once_until_it_is_written_again() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (b, c) = (temp.path().join("b"), temp.path().join("c"));
        write_map(&b, &["only-in-b"]);
        write_map(&c, &["only-in-c"]);

        let before = crate::store::read_index_calls();
        let first = SiblingContents::read([b.clone(), c.clone()]).expect("readable");
        assert_eq!(crate::store::read_index_calls() - before, 2);
        let again = SiblingContents::read([b.clone(), c.clone()]).expect("readable");
        assert_eq!(
            crate::store::read_index_calls() - before,
            2,
            "the second sweep decodes nothing"
        );
        assert!(first.holds("only-in-b") && again.holds("only-in-c"));

        write_map(&b, &["moved-to-b-and-longer"]);
        let rewritten = SiblingContents::read([b, c]).expect("readable");
        assert_eq!(
            crate::store::read_index_calls() - before,
            3,
            "only the rewritten map is decoded again"
        );
        assert!(rewritten.holds("moved-to-b-and-longer") && rewritten.holds("only-in-c"));
        assert!(
            !rewritten.holds("only-in-b"),
            "a cached answer does not outlive the write"
        );
    }

    #[test]
    fn an_unreadable_sibling_file_map_makes_liveness_unknown() {
        let temp = tempfile::tempdir().expect("tempdir");
        let damaged = temp.path().join("damaged");
        std::fs::create_dir_all(&damaged).expect("view dir");
        std::fs::write(damaged.join(INDEX_FILE), b"\xc1 not msgpack").expect("write");

        assert!(SiblingContents::read([damaged]).is_err(), "a sweep must not guess");
    }

    #[test]
    fn a_sibling_of_another_schema_holds_nothing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let older = temp.path().join("older");
        write_map(&older, &["h"]);
        let mut index = crate::store::read_index(&older).expect("read").expect("present");
        index.schema_ver -= 1;
        std::fs::write(older.join(INDEX_FILE), rmp_serde::to_vec_named(&index).expect("encode")).expect("write");

        let siblings = SiblingContents::read([older]).expect("another schema is not unknown");

        assert!(!siblings.holds("h"));
    }

    #[test]
    fn live_doc_keys_are_those_every_view_names_under_its_recorded_digest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (b, c, missing) = (temp.path().join("b"), temp.path().join("c"), temp.path().join("none"));
        write_index(&b, &doc_map(&[("in-b", "d1")]));
        write_index(&c, &doc_map(&[("shared", "d1"), ("in-c", "d2")]));
        let own = doc_map(&[("shared", "d1"), ("own", "d1")]);

        let keys = live_doc_keys_in(&own, [b, c, missing]).expect("readable");

        let expected: AHashSet<String> = [("in-b", "d1"), ("shared", "d1"), ("in-c", "d2"), ("own", "d1")]
            .into_iter()
            .map(|(hash, digest)| doc_key(hash, digest))
            .collect();
        assert_eq!(keys, expected);
        assert!(
            !keys.contains(&doc_key("in-c", "d1")),
            "the key is the digest the map recorded, not another"
        );
    }

    #[test]
    fn an_unreadable_sibling_file_map_leaves_live_doc_keys_unknown() {
        let temp = tempfile::tempdir().expect("tempdir");
        let damaged = temp.path().join("damaged");
        std::fs::create_dir_all(&damaged).expect("view dir");
        std::fs::write(damaged.join(INDEX_FILE), b"\xc1 not msgpack").expect("write");

        let unknown = live_doc_keys_in(&doc_map(&[("own", "d")]), [damaged]);

        assert!(
            matches!(unknown, Err(LivenessUnknown::UnreadableMap { .. })),
            "{unknown:?}"
        );
    }
}
