use super::{HashRefs, Index, IndexDb, Store, StoreError, VIEW_WORKING, ensure_dir};
use std::path::Path;

impl Store {
    pub(crate) fn open_base_scratch(root: &Path, scratch: &Path) -> Result<Self, StoreError> {
        let view_dir = scratch.to_path_buf();
        let blobs_dir = scratch.join(super::BLOBS_DIR);
        ensure_dir(&blobs_dir)?;
        let index_db = Some(IndexDb::open(&view_dir)?);
        let index = Index::empty();
        Ok(Self {
            root: root.to_path_buf(),
            basemind_dir: scratch.to_path_buf(),
            blobs_dir,
            blobs_shared: false,
            view_dir,
            view: VIEW_WORKING.to_string(),
            hash_refs: HashRefs::of(&index.files),
            index,
            index_db,
            #[cfg(feature = "intelligence")]
            lance: None,
            reset_stale_schema: false,
            index_rebuild_needed: false,
            _lock: None,
        })
    }
}
