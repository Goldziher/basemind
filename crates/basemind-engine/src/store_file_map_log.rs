//! The file map's delta log (`index.delta`): what an incremental pass writes instead of the whole
//! `index.msgpack`.
//!
//! The base file is rewritten only by a full write: the first flush of a store, a flush after the
//! map was replaced wholesale, and a compaction once the log has outgrown a share of the base. Every
//! other flush appends one frame holding the entries that changed since the previous flush plus the
//! small header fields, so the bytes written follow the batch, not the repository.
//!
//! A log belongs to exactly one base file: its first frame stamps that file (length, modification
//! time, inode). A reader applies the log only when the stamp matches the base it read, so a base
//! replaced by a compaction, a rebuild or anything else is never combined with a log of an earlier
//! one. Replaying a frame is idempotent (last writer per path wins), and a torn or damaged frame
//! ends the replay: the map then holds the last durable flush, which is what it may claim.
//!
//! A release that does not know the log shares the file map (same schema and blob layout) and reads
//! the base alone: the map of the last full write, so files the log changed since show as they were
//! then. Its scan compares that map with the working tree and indexes what differs, and a file's
//! rows are replaced by path, so the map and index it leaves are whole. It writes the base by
//! renaming a new file over it, which changes the base's identity: the log no longer matches, is
//! ignored by the next read here, and is removed by this binary's next full write (every store's
//! first flush is one). A store appends only while it holds the workspace's writer lock, which no
//! other writer of any release takes meanwhile, so no base is replaced under a log being appended to.

use std::fs::{Metadata, OpenOptions};
use std::io::Write;
use std::path::Path;

use ahash::AHashSet;
use serde::{Deserialize, Serialize};

use super::{DocEntry, FileEntry, INDEX_FILE, Index, StoreError};
use crate::path::RelPath;
use crate::store_doc_tombstone::DocTombstone;

/// The delta log's file name, next to [`INDEX_FILE`].
pub(crate) const LOG_FILE: &str = "index.delta";

/// A log below this size is never compacted: rewriting a small base costs about as much as it saves.
const COMPACT_FLOOR_BYTES: u64 = 1024 * 1024;
/// The log is compacted into the base once it exceeds this fraction of it (1 / n).
const COMPACT_BASE_DIVISOR: u64 = 4;
/// Bytes of the frame header: payload length (u32) and a truncated digest of the payload.
const FRAME_HEADER_LEN: usize = 4 + FRAME_DIGEST_LEN;
const FRAME_DIGEST_LEN: usize = 8;

/// Which base file a log belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct BaseStamp {
    len: u64,
    modified_ns: u64,
    inode: u64,
}

impl BaseStamp {
    /// The stamp of the file at `path`, whose metadata is `meta`.
    pub(super) fn of(meta: &Metadata, path: &Path) -> Self {
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
            .unwrap_or(0);
        Self {
            len: meta.len(),
            modified_ns,
            inode: file_identity(meta, path),
        }
    }

    pub(super) fn of_path(path: &Path) -> Option<Self> {
        std::fs::metadata(path).ok().map(|meta| Self::of(&meta, path))
    }
}

/// What tells one file from the file that replaced it at the same path: the inode on unix. On
/// Windows the file index, which is never reused while both files exist, because a replacement of
/// the same length written within the clock's tick has the same modification time too.
#[cfg(unix)]
fn file_identity(meta: &Metadata, _path: &Path) -> u64 {
    std::os::unix::fs::MetadataExt::ino(meta)
}

#[cfg(windows)]
fn file_identity(_meta: &Metadata, path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    file_id::get_file_id(path).map_or(0, |id| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        hasher.finish()
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_meta: &Metadata, _path: &Path) -> u64 {
    0
}

/// The on-disk state of a view's file map (base file and delta log), cheap to take and to compare:
/// two equal stamps of one view mean the file map has not been written in between. The log is
/// stamped by identity as well as length, so a log replaced by one of the same length differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewStamp {
    base: Option<BaseStamp>,
    log: Option<BaseStamp>,
}

impl ViewStamp {
    /// The stamp of the file map under `view_dir`; a view with no file map has a stamp of its own.
    pub(crate) fn of(view_dir: &Path) -> Self {
        Self {
            base: BaseStamp::of_path(&view_dir.join(INDEX_FILE)),
            log: BaseStamp::of_path(&view_dir.join(LOG_FILE)),
        }
    }
}

/// The fields of an [`Index`] other than its two file maps: written whole with every frame.
#[derive(Serialize)]
struct HeaderOut<'a> {
    schema_ver: u16,
    embed_policy: &'a str,
    doc_tombstones: &'a ahash::AHashMap<RelPath, DocTombstone>,
    generation: u64,
    rebuild_pending: bool,
    rebuild_pins: &'a [String],
    blob_layout: u8,
    group_incarnation: u64,
    view_id: u32,
}

#[derive(Deserialize)]
struct HeaderIn {
    schema_ver: u16,
    embed_policy: String,
    doc_tombstones: ahash::AHashMap<RelPath, DocTombstone>,
    generation: u64,
    rebuild_pending: bool,
    rebuild_pins: Vec<String>,
    blob_layout: u8,
    group_incarnation: u64,
    view_id: u32,
}

#[derive(Serialize)]
struct DeltaOut<'a> {
    header: HeaderOut<'a>,
    files: Vec<(&'a RelPath, &'a FileEntry)>,
    files_removed: Vec<&'a RelPath>,
    docs: Vec<(&'a RelPath, &'a DocEntry)>,
    docs_removed: Vec<&'a RelPath>,
}

#[derive(Deserialize)]
struct DeltaIn {
    header: HeaderIn,
    files: Vec<(RelPath, FileEntry)>,
    files_removed: Vec<RelPath>,
    docs: Vec<(RelPath, DocEntry)>,
    docs_removed: Vec<RelPath>,
}

impl DeltaIn {
    fn apply(self, index: &mut Index) {
        let Self {
            header,
            files,
            files_removed,
            docs,
            docs_removed,
        } = self;
        index.schema_ver = header.schema_ver;
        index.embed_policy = header.embed_policy;
        index.doc_tombstones = header.doc_tombstones;
        index.generation = header.generation;
        index.rebuild_pending = header.rebuild_pending;
        index.rebuild_pins = header.rebuild_pins;
        index.blob_layout = header.blob_layout;
        index.group_incarnation = header.group_incarnation;
        index.view_id = header.view_id;
        for rel in files_removed {
            index.files.remove(&rel);
        }
        index.files.extend(files);
        for rel in docs_removed {
            index.doc_files.remove(&rel);
        }
        index.doc_files.extend(docs);
    }
}

fn frame_digest(payload: &[u8]) -> [u8; FRAME_DIGEST_LEN] {
    let hash = crate::hashing::hash_bytes(payload);
    let mut digest = [0u8; FRAME_DIGEST_LEN];
    digest.copy_from_slice(&hash[..FRAME_DIGEST_LEN]);
    digest
}

fn push_frame(buf: &mut Vec<u8>, payload: &[u8]) -> Result<(), StoreError> {
    let len = u32::try_from(payload.len()).map_err(|_| StoreError::Io {
        path: LOG_FILE.into(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "file map delta frame over 4 GiB"),
    })?;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&frame_digest(payload));
    buf.extend_from_slice(payload);
    Ok(())
}

/// The payloads of the intact frames of `bytes`, in order; the first torn or damaged frame ends them.
fn frames(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        let (header, after) = rest.split_at_checked(FRAME_HEADER_LEN)?;
        let len = usize::try_from(u32::from_le_bytes(header[..4].try_into().ok()?)).ok()?;
        let (payload, tail) = after.split_at_checked(len)?;
        if header[4..] != frame_digest(payload) {
            return None;
        }
        rest = tail;
        Some(payload)
    })
}

/// Apply the delta log of `view_dir` to `index`, the file map read from the base file stamped
/// `base`. A log of another base, or none, leaves `index` as read; a log that exists but cannot be
/// read is an error, since the base alone would show files the log removed and miss the ones it
/// added.
pub(super) fn apply_log(view_dir: &Path, base: BaseStamp, index: &mut Index) -> Result<(), StoreError> {
    let log_path = view_dir.join(LOG_FILE);
    let bytes = match std::fs::read(&log_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(StoreError::Io { path: log_path, source }),
    };
    let mut payloads = frames(&bytes);
    let Some(stamp) = payloads
        .next()
        .and_then(|payload| rmp_serde::from_slice::<BaseStamp>(payload).ok())
    else {
        return Ok(());
    };
    if stamp != base {
        return Ok(());
    }
    for payload in payloads {
        match rmp_serde::from_slice::<DeltaIn>(payload) {
            Ok(delta) => delta.apply(index),
            Err(error) => {
                tracing::warn!(%error, "file map delta frame does not decode; the log is replayed up to it");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Remove the delta log of `view_dir`, if any. A log left behind is harmless (its stamp no longer
/// matches a replaced base), so a failure only logs.
pub(super) fn remove_log(view_dir: &Path) {
    match std::fs::remove_file(view_dir.join(LOG_FILE)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "could not remove the file map delta log"),
    }
}

/// The base file and log a store last wrote.
#[derive(Debug, Clone, Copy)]
struct LogState {
    base: BaseStamp,
    base_len: u64,
    log_len: u64,
}

impl LogState {
    fn compaction_due(&self) -> bool {
        self.log_len > COMPACT_FLOOR_BYTES && self.log_len > self.base_len / COMPACT_BASE_DIVISOR
    }
}

/// What a store has changed in its file map since the last flush, and what that flush left on disk.
#[derive(Debug)]
pub(super) struct FileMapState {
    dirty_files: AHashSet<RelPath>,
    dirty_docs: AHashSet<RelPath>,
    /// The entry counts the maps hold if every change went through the tracked mutators; a mismatch
    /// at flush means an entry was added or dropped behind them, and the flush writes the whole map.
    expected_files: usize,
    expected_docs: usize,
    /// The next flush writes the whole base file.
    full: bool,
    log: Option<LogState>,
    /// Bytes the flushes of this store wrote to the file map, base and log together.
    bytes_written: u64,
}

impl FileMapState {
    /// A store whose map has not been written yet: its first flush writes the base file.
    pub(super) fn new() -> Self {
        Self {
            dirty_files: AHashSet::new(),
            dirty_docs: AHashSet::new(),
            expected_files: 0,
            expected_docs: 0,
            full: true,
            log: None,
            bytes_written: 0,
        }
    }

    /// The next flush writes the whole base file: the map was replaced or emptied wholesale.
    pub(super) fn force_full(&mut self) {
        self.full = true;
    }

    pub(super) fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub(super) fn file_changed(&mut self, rel: RelPath, added: bool) {
        self.dirty_files.insert(rel);
        self.expected_files += usize::from(added);
    }

    #[cfg(feature = "code-search")]
    pub(super) fn file_edited(&mut self, rel: RelPath) {
        self.dirty_files.insert(rel);
    }

    pub(super) fn file_dropped(&mut self, rel: RelPath) {
        self.dirty_files.insert(rel);
        self.expected_files = self.expected_files.saturating_sub(1);
    }

    pub(super) fn doc_changed(&mut self, rel: RelPath, added: bool) {
        self.dirty_docs.insert(rel);
        self.expected_docs += usize::from(added);
    }

    pub(super) fn doc_dropped(&mut self, rel: RelPath) {
        self.dirty_docs.insert(rel);
        self.expected_docs = self.expected_docs.saturating_sub(1);
    }

    /// Make `index` durable as the file map of `view_dir`: one appended frame when only tracked
    /// entries changed since the last flush and `allow_delta`, else the whole base file. Returns the
    /// bytes written.
    pub(super) fn write(&mut self, view_dir: &Path, index: &Index, allow_delta: bool) -> Result<u64, StoreError> {
        let written = match self.delta_state(index).filter(|_| allow_delta) {
            Some(state) => self.append_delta(view_dir, state, index),
            None => self.write_base(view_dir, index),
        }?;
        self.bytes_written += written;
        Ok(written)
    }

    /// The log state a delta frame may be appended to, `None` when the whole map must be written.
    fn delta_state(&self, index: &Index) -> Option<LogState> {
        let state = self.log?;
        let tracked = index.files.len() == self.expected_files && index.doc_files.len() == self.expected_docs;
        (!self.full && tracked && !state.compaction_due()).then_some(state)
    }

    fn write_base(&mut self, view_dir: &Path, index: &Index) -> Result<u64, StoreError> {
        let base_len = super::file_map::write_file_map(view_dir, index)?;
        remove_log(view_dir);
        let stamp = BaseStamp::of_path(&view_dir.join(INDEX_FILE));
        self.log = stamp.map(|base| LogState {
            base,
            base_len,
            log_len: 0,
        });
        self.full = self.log.is_none();
        self.dirty_files.clear();
        self.dirty_docs.clear();
        self.expected_files = index.files.len();
        self.expected_docs = index.doc_files.len();
        Ok(base_len)
    }

    fn append_delta(&mut self, view_dir: &Path, state: LogState, index: &Index) -> Result<u64, StoreError> {
        let mut buf = Vec::new();
        let creating = state.log_len == 0;
        if creating {
            push_frame(&mut buf, &rmp_serde::to_vec_named(&state.base)?)?;
        }
        push_frame(&mut buf, &self.encode_delta(index)?)?;
        let log_path = view_dir.join(LOG_FILE);
        let appended = append_durably(view_dir, &log_path, &buf, creating);
        if let Err(source) = appended {
            // The log may end in a torn frame: the next flush starts over from a whole base file.
            self.full = true;
            return Err(StoreError::Io { path: log_path, source });
        }
        let written = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        self.log = Some(LogState {
            log_len: state.log_len + written,
            ..state
        });
        self.dirty_files.clear();
        self.dirty_docs.clear();
        Ok(written)
    }

    fn encode_delta(&self, index: &Index) -> Result<Vec<u8>, StoreError> {
        let mut files = Vec::new();
        let mut files_removed = Vec::new();
        for rel in &self.dirty_files {
            match index.files.get_key_value(rel) {
                Some((rel, entry)) => files.push((rel, entry)),
                None => files_removed.push(rel),
            }
        }
        let mut docs = Vec::new();
        let mut docs_removed = Vec::new();
        for rel in &self.dirty_docs {
            match index.doc_files.get_key_value(rel) {
                Some((rel, entry)) => docs.push((rel, entry)),
                None => docs_removed.push(rel),
            }
        }
        let delta = DeltaOut {
            header: HeaderOut {
                schema_ver: index.schema_ver,
                embed_policy: &index.embed_policy,
                doc_tombstones: &index.doc_tombstones,
                generation: index.generation,
                rebuild_pending: index.rebuild_pending,
                rebuild_pins: &index.rebuild_pins,
                blob_layout: index.blob_layout,
                group_incarnation: index.group_incarnation,
                view_id: index.view_id,
            },
            files,
            files_removed,
            docs,
            docs_removed,
        };
        Ok(rmp_serde::to_vec_named(&delta)?)
    }
}

/// Append `bytes` to the log (or start it over when `creating`) and fsync it, and the directory when
/// the file is new.
fn append_durably(view_dir: &Path, log_path: &Path, bytes: &[u8], creating: bool) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    if creating {
        options.create(true).write(true).truncate(true);
    } else {
        options.append(true);
    }
    let mut file = options.open(log_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if creating && let Err(error) = crate::store_layout::sync_directory(view_dir) {
        tracing::warn!(%error, "could not fsync the view directory after creating the file map delta log");
    }
    Ok(())
}
