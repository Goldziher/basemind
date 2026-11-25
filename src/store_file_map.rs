//! The file map on disk (`index.msgpack`): reading it under the version checks, writing it
//! atomically, and the per-view housekeeping around it. Split out of `store.rs` to keep that module
//! under the line cap; every name is re-exported there.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

use serde::Deserialize;

use super::file_map_log::BaseStamp;
use super::{INDEX_FILE, Index, StoreError, VIEW_WORKING, VIEWS_DIR, ensure_dir};
use crate::extract::SCHEMA_VER;

/// Write `index` as the view's file map: a temporary file, fsynced, renamed over the old one, and
/// the rename made durable. A failure leaves the previous file map in place. Returns the bytes written.
pub(super) fn write_file_map(view_dir: &Path, index: &Index) -> Result<u64, StoreError> {
    let final_path = view_dir.join(INDEX_FILE);
    let tmp_path = view_dir.join(format!("{INDEX_FILE}.tmp"));
    let bytes = rmp_serde::to_vec_named(index)?;
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|source| StoreError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        f.write_all(&bytes).map_err(|source| StoreError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        f.sync_all().map_err(|source| StoreError::Io {
            path: tmp_path.clone(),
            source,
        })?;
    }
    std::fs::rename(&tmp_path, &final_path).map_err(|source| StoreError::Io {
        path: final_path,
        source,
    })?;
    // The rename is durable only once the directory entry is. Not fatal: the new file map is
    // already in place, and some filesystems refuse to fsync a directory.
    if let Err(error) = crate::store_layout::sync_directory(view_dir) {
        tracing::warn!(%error, "could not fsync the view directory after writing the file map");
    }
    Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

/// How many set-aside file maps a view keeps whatever their age: the newest ones, in case one is
/// a newer basemind's map that is still wanted.
const KEEP_SET_ASIDE: usize = 2;

/// How old (seconds) a set-aside file map beyond [`KEEP_SET_ASIDE`] must be to be reaped.
const SET_ASIDE_HOLD_SECS: u64 = 7 * 24 * 60 * 60;

/// Rename the file map of `view_dir` aside as `index.msgpack.unreadable-<unix seconds>`: its
/// version cannot be read, so it may be a newer basemind's, which is never deleted at once. Older
/// set-asides beyond the newest [`KEEP_SET_ASIDE`] are reaped once they are a week old.
pub(super) fn set_aside_unreadable_map(view_dir: &Path) -> Result<(), StoreError> {
    let from = view_dir.join(INDEX_FILE);
    let secs = unix_secs_now();
    let to = view_dir.join(format!("{INDEX_FILE}.unreadable-{secs}"));
    std::fs::rename(&from, &to).map_err(|source| StoreError::Io { path: from, source })?;
    super::file_map_log::remove_log(view_dir);
    tracing::warn!(path = %to.display(), "file map unreadable; set aside, and the view is rebuilt");
    reap_set_aside_maps(view_dir, secs, KEEP_SET_ASIDE, SET_ASIDE_HOLD_SECS);
    Ok(())
}

fn unix_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Delete the `index.msgpack.unreadable-<secs>` files of `view_dir` other than the newest `keep`,
/// when they are at least `hold` seconds older than `now`. Best effort: a failure is logged, since
/// a leftover costs only disk.
fn reap_set_aside_maps(view_dir: &Path, now: u64, keep: usize, hold: u64) {
    let prefix = format!("{INDEX_FILE}.unreadable-");
    let Ok(entries) = std::fs::read_dir(view_dir) else {
        return;
    };
    let mut set_aside: Vec<(u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let secs = name.to_str()?.strip_prefix(&prefix)?.parse::<u64>().ok()?;
            Some((secs, entry.path()))
        })
        .collect();
    set_aside.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    for (secs, path) in set_aside.into_iter().skip(keep) {
        if now.saturating_sub(secs) < hold {
            continue;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(%error, path = %path.display(), "could not reap a set-aside file map");
        }
    }
}

/// Delete the index file in a single view's directory.
pub(crate) fn wipe_view(view_dir: &Path) -> Result<(), StoreError> {
    let index_path = view_dir.join(INDEX_FILE);
    if index_path.exists() {
        std::fs::remove_file(&index_path).map_err(|source| StoreError::Io {
            path: index_path,
            source,
        })?;
    }
    super::file_map_log::remove_log(view_dir);
    Ok(())
}

/// Pre-views installs kept `index.msgpack` at the top of `.basemind/`. After the upgrade,
/// each view lives under `.basemind/views/<view>/`. If we detect the legacy file AND no
/// working-view file exists yet, move it in place. Idempotent: re-runs are no-ops.
pub(super) fn migrate_legacy_index_into_views(basemind_dir: &Path) -> Result<(), StoreError> {
    let legacy = basemind_dir.join(INDEX_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    let working_dir = basemind_dir.join(VIEWS_DIR).join(VIEW_WORKING);
    let working_index = working_dir.join(INDEX_FILE);
    if working_index.exists() {
        let _ = std::fs::remove_file(&legacy);
        return Ok(());
    }
    ensure_dir(&working_dir)?;
    std::fs::rename(&legacy, &working_index).map_err(|source| StoreError::Io {
        path: working_index,
        source,
    })?;
    tracing::info!("migrated .basemind/index.msgpack → .basemind/views/{VIEW_WORKING}/index.msgpack");
    Ok(())
}

/// Read and deserialize a view's `index.msgpack`. `Ok(None)` when the file is absent;
/// `Err(StoreError::SchemaMismatch)` on an older version, `Err(StoreError::NewerSchema)` on a newer
/// schema or blob layout, `Err(StoreError::StaleBlobLayout)` on a map of an earlier blob store
/// layout, `Err(StoreError::Decode)` on a map of this version that does not decode, and
/// `Err(StoreError::UnreadableFileMap)` on one whose version cannot be read. Reused by `store_gc` to
/// enumerate the live blob hashes referenced by every view.
/// The fields that say which basemind wrote a file map: `schema_ver`, which every layout has had,
/// and `blob_layout`, absent (`0`) before blob namespaces.
#[derive(Deserialize)]
struct Version {
    schema_ver: u16,
    #[serde(default)]
    blob_layout: u8,
}

/// Whether the file map of `view_dir` may be a newer basemind's: its version fields read a newer
/// schema or blob layout, or cannot be read at all. Reads the fields alone, never the whole map
/// into memory. `false` when there is no map.
pub(crate) fn written_by_newer(view_dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(view_dir.join(INDEX_FILE)) else {
        return false;
    };
    match rmp_serde::from_slice::<Version>(&bytes) {
        Ok(version) => version.schema_ver > SCHEMA_VER || version.blob_layout > crate::store_layout::BLOB_LAYOUT,
        Err(_) => true,
    }
}

/// Reads of a map replaced by a compaction while it was read are retried this often.
const READ_ATTEMPTS: u32 = 3;

#[cfg(test)]
thread_local! {
    static READ_INDEX_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times this thread called [`read_index`] since it started.
#[cfg(test)]
pub(crate) fn read_index_calls() -> usize {
    READ_INDEX_CALLS.with(std::cell::Cell::get)
}

// Run between a read's apply of the delta log and its settle check, standing in for a compaction
// that lands there.
#[cfg(test)]
thread_local! {
    static BEFORE_SETTLE_CHECK: std::cell::Cell<Option<fn(&Path)>> = const { std::cell::Cell::new(None) };
}

/// The view's file map: the base file with its delta log applied (see [`super::file_map_log`]).
///
/// A base replaced between its read and the end of the replay (a compaction, which then removes
/// the log) may have lost the log's durable updates, so the read is retried; one that never
/// settles within [`READ_ATTEMPTS`] is [`StoreError::FileMapUnsettled`], never the possibly
/// incomplete map.
pub(crate) fn read_index(view_dir: &Path) -> Result<Option<Index>, StoreError> {
    #[cfg(test)]
    READ_INDEX_CALLS.with(|calls| calls.set(calls.get() + 1));
    let path = view_dir.join(INDEX_FILE);
    for _ in 0..READ_ATTEMPTS {
        if !path.exists() {
            return Ok(None);
        }
        let (mut index, stamp) = read_base(&path)?;
        super::file_map_log::apply_log(view_dir, stamp, &mut index)?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_SETTLE_CHECK.with(std::cell::Cell::get) {
            hook(&path);
        }
        // A compaction between the read and now replaced the base the log was applied to. ~keep
        if BaseStamp::of_path(&path) == Some(stamp) {
            return Ok(Some(index));
        }
    }
    Err(StoreError::FileMapUnsettled {
        path,
        attempts: READ_ATTEMPTS,
    })
}

/// Decode the base file at `path`, with the stamp of the very file read.
fn read_base(path: &Path) -> Result<(Index, BaseStamp), StoreError> {
    let io_error = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = std::fs::File::open(path).map_err(io_error)?;
    let meta = file.metadata().map_err(io_error)?;
    let stamp = BaseStamp::of(&meta, path);
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.read_to_end(&mut bytes).map_err(io_error)?;
    // The version is read on its own first: a newer map need not decode as this one, and a decode
    // failure must not read as a damaged map that may be removed. Only a map whose version reads as
    // this binary's is rebuildable when the rest does not decode. ~keep
    let Ok(Version {
        schema_ver,
        blob_layout,
    }) = rmp_serde::from_slice(&bytes)
    else {
        return Err(StoreError::UnreadableFileMap {
            path: path.to_path_buf(),
        });
    };
    check_schema(schema_ver)?;
    if blob_layout > crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::NewerSchema {
            stored: u16::from(blob_layout),
            current: u16::from(crate::store_layout::BLOB_LAYOUT),
        });
    }
    let index: Index = rmp_serde::from_slice(&bytes)?;
    check_schema(index.schema_ver)?;
    if index.blob_layout > crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::NewerSchema {
            stored: u16::from(index.blob_layout),
            current: u16::from(crate::store_layout::BLOB_LAYOUT),
        });
    }
    if index.blob_layout < crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::StaleBlobLayout {
            found: index.blob_layout,
            expected: crate::store_layout::BLOB_LAYOUT,
        });
    }
    Ok((index, stamp))
}

/// What a writer open ([`super::Store::open_with_holder`]) does with a view's file map. The one
/// decision both that open and `basemind upgrade --plan` ([`crate::upgrade_plan`]) act on, so the
/// plan cannot name a different set of views than the open then rebuilds.
#[derive(Debug)]
pub(crate) enum ViewOnOpen {
    /// No file map: the view starts empty.
    Absent,
    /// A map of this binary's schema and blob layout: kept. Boxed, being far larger than the rest.
    Keep(Box<Index>),
    /// A map of an older schema (a release-minor bump): removed, and every file extracted again.
    OlderSchema { found: u16 },
    /// A map of an earlier blob layout, or one of this version that does not decode, or one whose
    /// version cannot be read: discarded ([`sets_aside`] says whether it is removed or renamed), and
    /// every file extracted again.
    Discard(StoreError),
    /// The open fails: the map is a newer basemind's, or could not be read at all. Nothing is touched.
    Refuse(StoreError),
}

/// Decide what a writer open does with the file map of `view_dir`. Reads only.
pub(crate) fn view_on_open(view_dir: &Path) -> ViewOnOpen {
    match read_index(view_dir) {
        Ok(Some(index)) => ViewOnOpen::Keep(Box::new(index)),
        Ok(None) => ViewOnOpen::Absent,
        Err(StoreError::SchemaMismatch { found, .. }) => ViewOnOpen::OlderSchema { found },
        // A map that does not decode is rebuildable cache; refusing it left the workspace unopenable
        // until someone cleared the cache. Its index goes too: no map lists the files of its rows. ~keep
        Err(
            error @ (StoreError::StaleBlobLayout { .. } | StoreError::Decode(_) | StoreError::UnreadableFileMap { .. }),
        ) => ViewOnOpen::Discard(error),
        Err(error) => ViewOnOpen::Refuse(error),
    }
}

/// Whether a discarded file map ([`ViewOnOpen::Discard`]) is renamed aside rather than removed: its
/// version cannot be read, so it may be a newer basemind's, which is never deleted.
pub(crate) fn sets_aside(why: &StoreError) -> bool {
    matches!(why, StoreError::UnreadableFileMap { .. })
}

/// Guard every blob / index read against a foreign on-disk schema: an older one is a rebuildable
/// [`StoreError::SchemaMismatch`], a newer one a [`StoreError::NewerSchema`] no caller may wipe.
/// `pub(crate)` because the blob accessors that call it live in [`crate::store_blob`].
pub(crate) fn check_schema(found: u16) -> Result<(), StoreError> {
    match found.cmp(&SCHEMA_VER) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(StoreError::SchemaMismatch {
            found,
            expected: SCHEMA_VER,
        }),
        std::cmp::Ordering::Greater => Err(StoreError::NewerSchema {
            stored: found,
            current: SCHEMA_VER,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    fn plant(dir: &Path, secs: u64) -> std::path::PathBuf {
        let path = dir.join(format!("{INDEX_FILE}.unreadable-{secs}"));
        std::fs::write(&path, b"x").expect("plant");
        path
    }

    fn survivors(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    thread_local! {
        static COMPACTIONS_LEFT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// Replace the base at `path` by one a generation on, as a compaction does, while any of the
    /// compactions the test planned are left.
    fn compact_under_the_read(path: &Path) {
        let left = COMPACTIONS_LEFT.with(std::cell::Cell::get);
        if left == 0 {
            return;
        }
        COMPACTIONS_LEFT.with(|cell| cell.set(left - 1));
        let view_dir = path.parent().expect("a view dir");
        let (mut index, _) = read_base(path).expect("read the base");
        index.generation += 1;
        write_file_map(view_dir, &index).expect("replace the base");
        super::super::file_map_log::remove_log(view_dir);
    }

    /// The generation `read_index` returns when `compactions` reads in a row see their base replaced
    /// under them, or the error it fails with.
    fn read_under_compactions(compactions: u32) -> Result<u64, StoreError> {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file_map(tmp.path(), &Index::empty()).expect("write the base");
        COMPACTIONS_LEFT.with(|cell| cell.set(compactions));
        BEFORE_SETTLE_CHECK.with(|hook| hook.set(Some(compact_under_the_read)));
        let read = read_index(tmp.path());
        BEFORE_SETTLE_CHECK.with(|hook| hook.set(None));
        read.map(|index| index.expect("a file map").generation)
    }

    /// A read whose base a compaction replaced is retried and returns the settled map. One that
    /// never settles used to return the last, possibly incomplete map on its final attempt; it is
    /// an error instead.
    #[test]
    fn a_read_that_never_settles_fails_instead_of_returning_an_unsettled_map() {
        for compactions in [0, 1, READ_ATTEMPTS - 1] {
            assert_eq!(
                read_under_compactions(compactions).expect("a read that settles"),
                u64::from(compactions),
                "{compactions} compactions under the read"
            );
        }
        let error = read_under_compactions(READ_ATTEMPTS).expect_err("a read that never settles");
        assert!(
            matches!(error, StoreError::FileMapUnsettled { attempts, .. } if attempts == READ_ATTEMPTS),
            "{error:?}"
        );
    }

    #[test]
    fn reaping_keeps_the_newest_and_anything_inside_the_hold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = 100 * DAY;
        for age_days in [60, 30, 20, 10, 8, 3, 1] {
            plant(tmp.path(), now - age_days * DAY);
        }
        std::fs::write(tmp.path().join(INDEX_FILE), b"live").expect("live map");
        std::fs::write(tmp.path().join(format!("{INDEX_FILE}.unreadable-notasecs")), b"x").expect("odd");

        reap_set_aside_maps(tmp.path(), now, 2, 7 * DAY);

        let expect: Vec<String> = [
            INDEX_FILE.to_string(),
            format!("{INDEX_FILE}.unreadable-notasecs"),
            format!("{INDEX_FILE}.unreadable-{}", now - DAY),
            format!("{INDEX_FILE}.unreadable-{}", now - 3 * DAY),
        ]
        .into_iter()
        .collect();
        let mut expect = expect;
        expect.sort();
        assert_eq!(survivors(tmp.path()), expect);
    }

    #[test]
    fn reaping_keeps_old_set_asides_within_the_keep_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = 100 * DAY;
        let old = plant(tmp.path(), now - 90 * DAY);
        let older = plant(tmp.path(), now - 95 * DAY);
        reap_set_aside_maps(tmp.path(), now, 2, 7 * DAY);
        assert!(old.exists() && older.exists());
    }

    #[test]
    fn setting_a_map_aside_reaps_the_stale_ones_beyond_the_keep_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = unix_secs_now();
        let stale: Vec<_> = (1..=3).map(|n| plant(tmp.path(), now - 30 * DAY - n)).collect();
        std::fs::write(tmp.path().join(INDEX_FILE), b"torn").expect("map");

        set_aside_unreadable_map(tmp.path()).expect("set aside");

        let remaining = stale.iter().filter(|path| path.exists()).count();
        assert_eq!(
            remaining,
            KEEP_SET_ASIDE - 1,
            "the new set-aside and one old one are the kept two"
        );
        assert!(!tmp.path().join(INDEX_FILE).exists());
    }
}
