//! A keyspace as one view sees it: the whole keyspace, or the rows under the view's 4-byte prefix.
//!
//! A view with a database of its own reads and writes its keyspaces as they are. In a group index
//! (ADR-0028) every view of the repository group shares one database, and the rows that describe a
//! path keep their path keys under the view's id. [`Scoped`] adds that prefix to every key it is
//! given and strips it from every key it returns, so the code above it is the same in both layouts.

use std::ops::Bound;

use fjall::{Keyspace, OwnedWriteBatch, Slice};

use super::IndexError;
use super::view::{IndexBytes, KvIter, KvResult};

/// Bytes of the view prefix in a group index.
pub const VIEW_PREFIX_LEN: usize = 4;

/// The 4-byte key prefix of the view with id `view_id`.
pub fn view_prefix(view_id: u32) -> [u8; VIEW_PREFIX_LEN] {
    view_id.to_be_bytes()
}

/// One keyspace, scoped to a view. Cheap to clone: the keyspace handle is reference counted.
#[derive(Clone)]
pub struct Scoped {
    keyspace: Keyspace,
    prefix: Option<[u8; VIEW_PREFIX_LEN]>,
}

impl Scoped {
    /// Every row of `keyspace`: a database of the view's own, or a keyspace shared by the group.
    pub(crate) fn whole(keyspace: Keyspace) -> Self {
        Self { keyspace, prefix: None }
    }

    /// The rows of `keyspace` under the view prefix of `view_id`.
    pub(crate) fn view(keyspace: Keyspace, view_id: u32) -> Self {
        Self {
            keyspace,
            prefix: Some(view_prefix(view_id)),
        }
    }

    /// The fjall keyspace underneath, for what only fjall does to a whole keyspace (memtable
    /// rotation). Never read or write rows through it: its keys carry the prefix.
    pub(crate) fn raw(&self) -> &Keyspace {
        &self.keyspace
    }

    /// Whether this handle sees a part of the keyspace that other views share.
    pub(crate) fn is_view_scoped(&self) -> bool {
        self.prefix.is_some()
    }

    /// The stored form of `key`.
    pub(crate) fn full_key(&self, key: impl Into<Vec<u8>>) -> Vec<u8> {
        let key = key.into();
        match &self.prefix {
            None => key,
            Some(prefix) => {
                let mut full = Vec::with_capacity(VIEW_PREFIX_LEN + key.len());
                full.extend_from_slice(prefix);
                full.extend_from_slice(&key);
                full
            }
        }
    }

    fn strip(&self, key: Slice) -> IndexBytes {
        IndexBytes::from_fjall_at(key, self.prefix.map_or(0, |_| VIEW_PREFIX_LEN))
    }

    fn row(&self, guard: fjall::Guard) -> KvResult {
        let (key, value) = guard.into_inner().map_err(IndexError::from)?;
        Ok((self.strip(key), IndexBytes::from_fjall(value)))
    }

    fn key_only(&self, guard: fjall::Guard) -> Result<IndexBytes, IndexError> {
        Ok(self.strip(guard.key().map_err(IndexError::from)?))
    }

    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        // A whole keyspace reads the key as given, without copying it. ~keep
        let found = match &self.prefix {
            None => self.keyspace.get(key)?,
            Some(_) => self.keyspace.get(self.full_key(key))?,
        };
        Ok(found.map(IndexBytes::from_fjall))
    }

    pub(crate) fn insert(&self, key: &[u8], value: impl Into<Slice>) -> Result<(), IndexError> {
        Ok(self.keyspace.insert(self.full_key(key), value)?)
    }

    pub(crate) fn remove(&self, key: &[u8]) -> Result<(), IndexError> {
        Ok(self.keyspace.remove(self.full_key(key))?)
    }

    /// Stage an insert of `key` into `batch`, returning the stored key's length.
    pub(crate) fn stage_insert(&self, batch: &mut OwnedWriteBatch, key: impl Into<Vec<u8>>, value: Slice) -> usize {
        let key = self.full_key(key);
        let len = key.len();
        batch.insert(&self.keyspace, key, value);
        len
    }

    /// Stage a delete of `key` into `batch`, returning the stored key's length.
    pub(crate) fn stage_remove(&self, batch: &mut OwnedWriteBatch, key: impl Into<Vec<u8>>) -> usize {
        let key = self.full_key(key);
        let len = key.len();
        batch.remove(&self.keyspace, key);
        len
    }

    /// Rows whose key starts with `prefix`, in key order.
    pub(crate) fn prefix<'a>(&'a self, prefix: &[u8]) -> KvIter<'a> {
        let rows = match &self.prefix {
            None => self.keyspace.prefix(prefix),
            Some(_) => self.keyspace.prefix(self.full_key(prefix)),
        };
        Box::new(rows.map(|guard| self.row(guard)))
    }

    /// Keys starting with `prefix`, in key order, without reading their values.
    #[cfg(feature = "code-search")]
    pub(crate) fn prefix_keys<'a>(
        &'a self,
        prefix: &[u8],
    ) -> impl Iterator<Item = Result<IndexBytes, IndexError>> + 'a {
        self.keyspace
            .prefix(self.full_key(prefix))
            .map(|guard| self.key_only(guard))
    }

    /// The stored bounds of a range given in view keys. An unbounded side stays inside the prefix.
    fn bounds(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
        let scope = |bound: Bound<&[u8]>| match bound {
            Bound::Included(key) => Bound::Included(self.full_key(key)),
            Bound::Excluded(key) => Bound::Excluded(self.full_key(key)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let (mut lower, mut upper) = (scope(lower), scope(upper));
        if let Some(prefix) = &self.prefix {
            if matches!(lower, Bound::Unbounded) {
                lower = Bound::Included(prefix.to_vec());
            }
            if matches!(upper, Bound::Unbounded) {
                upper = super::name_dict::upper_bound(prefix).map_or(Bound::Unbounded, Bound::Excluded);
            }
        }
        (lower, upper)
    }

    /// Rows between the bounds, in key order.
    pub(crate) fn range<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'a> {
        let bounds = self.bounds(lower, upper);
        Box::new(self.keyspace.range::<Vec<u8>, _>(bounds).map(|guard| self.row(guard)))
    }

    /// Every row, in key order.
    pub(crate) fn iter<'a>(&'a self) -> KvIter<'a> {
        self.range(Bound::Unbounded, Bound::Unbounded)
    }

    /// Every key, in key order, without reading values.
    pub(crate) fn keys<'a>(&'a self) -> impl Iterator<Item = Result<IndexBytes, IndexError>> + 'a {
        let bounds = self.bounds(Bound::Unbounded, Bound::Unbounded);
        self.keyspace
            .range::<Vec<u8>, _>(bounds)
            .map(|guard| self.key_only(guard))
    }

    /// Whether the scope holds no row.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> Result<bool, IndexError> {
        Ok(self.keys().next().transpose()?.is_none())
    }

    /// Delete every row of the scope. A whole keyspace is cleared at once; a view's rows are
    /// deleted in batches of [`CLEAR_BATCH_ROWS`], each committed before the next is read.
    pub(crate) fn clear(&self, db: &fjall::Database) -> Result<(), IndexError> {
        if self.prefix.is_none() {
            return Ok(self.keyspace.clear()?);
        }
        loop {
            let keys: Vec<IndexBytes> = self.keys().take(CLEAR_BATCH_ROWS).collect::<Result<_, _>>()?;
            if keys.is_empty() {
                return Ok(());
            }
            let mut batch = db.batch();
            for key in &keys {
                self.stage_remove(&mut batch, key.to_vec());
            }
            batch.commit()?;
        }
    }
}

/// Rows of a view deleted per batch by [`Scoped::clear`].
const CLEAR_BATCH_ROWS: usize = 10_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn keyspace() -> (tempfile::TempDir, fjall::Database, Keyspace) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = fjall::Database::builder(dir.path()).open().expect("open");
        let keyspace = db
            .keyspace("rows", fjall::KeyspaceCreateOptions::default)
            .expect("keyspace");
        (dir, db, keyspace)
    }

    fn keys(rows: KvIter<'_>) -> Vec<Vec<u8>> {
        rows.map(|row| row.expect("row").0.to_vec()).collect()
    }

    #[test]
    fn two_views_see_only_their_own_rows_under_the_same_keys() {
        let (_dir, db, keyspace) = keyspace();
        let (a, b) = (Scoped::view(keyspace.clone(), 1), Scoped::view(keyspace.clone(), 2));
        a.insert(b"k1", "a1").expect("insert");
        a.insert(b"k2", "a2").expect("insert");
        b.insert(b"k1", "b1").expect("insert");

        assert_eq!(keys(a.iter()), vec![b"k1".to_vec(), b"k2".to_vec()]);
        assert_eq!(keys(b.iter()), vec![b"k1".to_vec()]);
        assert_eq!(&*b.get(b"k1").expect("get").expect("present"), b"b1");
        assert!(b.get(b"k2").expect("get").is_none());
        assert_eq!(keys(a.prefix(b"k")), vec![b"k1".to_vec(), b"k2".to_vec()]);
        assert_eq!(
            keys(a.range(Bound::Excluded(b"k1"), Bound::Unbounded)),
            vec![b"k2".to_vec()]
        );

        a.clear(&db).expect("clear");
        assert!(a.is_empty().expect("empty"));
        assert_eq!(
            keys(b.iter()),
            vec![b"k1".to_vec()],
            "clearing one view leaves the other"
        );
        assert_eq!(keys(Scoped::whole(keyspace).iter()).len(), 1);
    }

    #[test]
    fn a_whole_keyspace_reads_and_writes_keys_as_given() {
        let (_dir, _db, keyspace) = keyspace();
        let whole = Scoped::whole(keyspace.clone());
        whole.insert(b"k", "v").expect("insert");
        assert_eq!(&*keyspace.get(b"k").expect("get").expect("present"), b"v");
        assert_eq!(keys(whole.iter()), vec![b"k".to_vec()]);
    }

    #[test]
    fn the_last_view_id_has_an_upper_bound() {
        let (_dir, _db, keyspace) = keyspace();
        let last = Scoped::view(keyspace.clone(), u32::MAX);
        last.insert(b"k", "v").expect("insert");
        assert_eq!(keys(last.iter()), vec![b"k".to_vec()]);
    }
}
