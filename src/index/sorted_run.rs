use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

const MAGIC: &[u8; 8] = b"BMSRUN01";
const OFFSET_BYTES: usize = 8;
const LENGTH_BYTES: usize = 4;

fn invalid_run() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid sorted run")
}

pub fn write_run(
    path: &Path,
    rows: impl Iterator<Item = io::Result<(impl AsRef<[u8]>, impl AsRef<[u8]>)>>,
) -> io::Result<()> {
    let mut writer = BufWriter::new(std::fs::OpenOptions::new().write(true).create_new(true).open(path)?);
    writer.write_all(MAGIC)?;
    let mut offsets = Vec::new();
    let mut offset = MAGIC.len() as u64;
    let mut previous = None::<Vec<u8>>;
    for row in rows {
        let (key, value) = row?;
        let (key, value) = (key.as_ref(), value.as_ref());
        if previous.as_deref().is_some_and(|previous| previous >= key) {
            return Err(invalid_run());
        }
        let previous = previous.get_or_insert_with(Vec::new);
        previous.clear();
        previous.extend_from_slice(key);
        let key_len = u32::try_from(key.len()).map_err(|_| invalid_run())?;
        offsets.push(offset);
        writer.write_all(&key_len.to_le_bytes())?;
        writer.write_all(key)?;
        writer.write_all(value)?;
        offset = offset
            .checked_add(LENGTH_BYTES as u64)
            .and_then(|offset| offset.checked_add(key.len() as u64))
            .and_then(|offset| offset.checked_add(value.len() as u64))
            .ok_or_else(invalid_run)?;
    }
    offsets.push(offset);
    for position in offsets {
        writer.write_all(&position.to_le_bytes())?;
    }
    writer.write_all(&offset.to_le_bytes())?;
    writer.flush()?;
    writer.get_ref().sync_all()
}

pub(super) struct Mapping {
    pub(super) mmap: Mmap,
    _lease: Option<Arc<crate::store_base_retention::GenerationLease>>,
}

pub struct SortedRun {
    data: Arc<Mapping>,
    table: usize,
    rows: usize,
}

impl SortedRun {
    /// # Safety
    /// The caller must keep the file unchanged and untruncated for the mapping's lifetime.
    pub unsafe fn open(path: &Path) -> io::Result<Self> {
        unsafe { Self::open_inner(path, None) }
    }

    unsafe fn open_inner(
        path: &Path,
        lease: Option<Arc<crate::store_base_retention::GenerationLease>>,
    ) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: The caller guarantees an immutable file for the lifetime of the mapping.
        let data = unsafe { Mmap::map(&file)? };
        let footer = data.len().checked_sub(OFFSET_BYTES).ok_or_else(invalid_run)?;
        let table = read_offset(&data, footer).ok_or_else(invalid_run)?;
        let offsets = footer.checked_sub(table).ok_or_else(invalid_run)?;
        if !data.starts_with(MAGIC)
            || table < MAGIC.len()
            || offsets < OFFSET_BYTES
            || !offsets.is_multiple_of(OFFSET_BYTES)
            || read_offset(&data, footer - OFFSET_BYTES) != Some(table)
        {
            return Err(invalid_run());
        }
        let rows = offsets / OFFSET_BYTES - 1;
        Ok(Self {
            data: Arc::new(Mapping {
                mmap: data,
                _lease: lease,
            }),
            table,
            rows,
        })
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    pub fn mapped_bytes(&self) -> usize {
        self.data.mmap.len()
    }

    pub fn row(&self, index: usize) -> io::Result<(&[u8], &[u8])> {
        if index >= self.rows {
            return Err(invalid_run());
        }
        let slot = self.table + index * OFFSET_BYTES;
        let start = read_offset(&self.data.mmap, slot).ok_or_else(invalid_run)?;
        let end = read_offset(&self.data.mmap, slot + OFFSET_BYTES).ok_or_else(invalid_run)?;
        if start < MAGIC.len() || end > self.table {
            return Err(invalid_run());
        }
        let row = self.data.mmap.get(start..end).ok_or_else(invalid_run)?;
        let length = row.get(..LENGTH_BYTES).ok_or_else(invalid_run)?;
        let key_len = u32::from_le_bytes(length.try_into().map_err(|_| invalid_run())?) as usize;
        row[LENGTH_BYTES..].split_at_checked(key_len).ok_or_else(invalid_run)
    }

    pub fn get(&self, key: &[u8]) -> io::Result<Option<&[u8]>> {
        let index = self.lower_bound(key)?;
        if index == self.rows {
            return Ok(None);
        }
        let (found, value) = self.row(index)?;
        Ok((found == key).then_some(value))
    }

    pub fn prefix(&self, prefix: &[u8]) -> io::Result<std::ops::Range<usize>> {
        let end = super::name_dict::upper_bound(prefix);
        self.range(
            Bound::Included(prefix),
            end.as_deref().map_or(Bound::Unbounded, Bound::Excluded),
        )
    }

    pub fn range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> io::Result<std::ops::Range<usize>> {
        let start = match lower {
            Bound::Unbounded => 0,
            Bound::Included(key) => self.lower_bound(key)?,
            Bound::Excluded(key) => self.upper_bound(key)?,
        };
        let end = match upper {
            Bound::Unbounded => self.rows,
            Bound::Included(key) => self.upper_bound(key)?,
            Bound::Excluded(key) => self.lower_bound(key)?,
        };
        Ok(start..end)
    }

    fn upper_bound(&self, key: &[u8]) -> io::Result<usize> {
        let index = self.lower_bound(key)?;
        if index < self.rows && self.row(index)?.0 == key {
            return Ok(index + 1);
        }
        Ok(index)
    }

    pub(super) fn lower_bound(&self, key: &[u8]) -> io::Result<usize> {
        let mut start = 0;
        let mut end = self.rows;
        while start < end {
            let middle = start + (end - start) / 2;
            if self.row(middle)?.0 < key {
                start = middle + 1;
            } else {
                end = middle;
            }
        }
        Ok(start)
    }
}

fn read_offset(bytes: &[u8], start: usize) -> Option<usize> {
    let end = start.checked_add(OFFSET_BYTES)?;
    let position = u64::from_le_bytes(bytes.get(start..end)?.try_into().ok()?);
    usize::try_from(position).ok()
}

/// An immutable set of sorted runs that implements the ordinary index read contract.
pub struct SortedRunView {
    directory: PathBuf,
    runs: std::collections::BTreeMap<super::view::Part, SortedRun>,
}

use super::IndexError;
use super::view::{IndexBytes, IndexView, KvIter, Part};

impl SortedRunView {
    /// Export every readable partition into a pre-created, empty generation directory.
    pub fn export(directory: &Path, view: &dyn IndexView) -> io::Result<()> {
        for part in Part::ALL {
            write_run(
                &directory.join(part.name()),
                view.iter(part).map(|row| row.map_err(io::Error::other)),
            )?;
        }
        Ok(())
    }

    /// # Safety
    /// The caller must keep every run file immutable while this view or any returned bytes live.
    pub unsafe fn open(directory: &Path) -> io::Result<Self> {
        let runs = Part::ALL
            .into_iter()
            .map(|part| {
                // SAFETY: The caller guarantees immutability for each run and returned handle.
                unsafe { SortedRun::open(&directory.join(part.name())) }.map(|run| (part, run))
            })
            .collect::<io::Result<_>>()?;
        Ok(Self {
            directory: directory.to_path_buf(),
            runs,
        })
    }

    pub(crate) fn open_leased(
        lease: Arc<crate::store_base_retention::GenerationLease>,
        subdir: &str,
    ) -> io::Result<Self> {
        let directory = lease.path().join(subdir);
        let runs = Part::ALL
            .into_iter()
            .map(|part| {
                let path = directory.join(part.name());
                // SAFETY: Published run files are immutable and GC cannot retire the generation while leased.
                unsafe { SortedRun::open_inner(&path, Some(Arc::clone(&lease))) }.map(|run| (part, run))
            })
            .collect::<io::Result<_>>()?;
        Ok(Self { directory, runs })
    }

    fn error(&self, part: Part, source: io::Error) -> IndexError {
        IndexError::Io {
            path: self.directory.join(part.name()),
            source,
        }
    }

    fn rows(&self, part: Part, range: io::Result<std::ops::Range<usize>>) -> KvIter<'_> {
        let range = match range {
            Ok(range) => range,
            Err(error) => return Box::new(std::iter::once(Err(self.error(part, error)))),
        };
        let run = &self.runs[&part];
        Box::new(range.map(move |index| {
            let (key, value) = run.row(index).map_err(|error| self.error(part, error))?;
            Ok((run.bytes(key), run.bytes(value)))
        }))
    }
}

impl SortedRun {
    fn bytes(&self, bytes: &[u8]) -> IndexBytes {
        let start = bytes.as_ptr() as usize - self.data.mmap.as_ptr() as usize;
        IndexBytes::from_mapping(Arc::clone(&self.data), start..start + bytes.len())
    }
}

impl IndexView for SortedRunView {
    fn get(&self, part: Part, key: &[u8]) -> Result<Option<IndexBytes>, IndexError> {
        let run = &self.runs[&part];
        run.get(key)
            .map(|value| value.map(|value| run.bytes(value)))
            .map_err(|error| self.error(part, error))
    }

    fn prefix(&self, part: Part, prefix: &[u8]) -> KvIter<'_> {
        self.rows(part, self.runs[&part].prefix(prefix))
    }

    fn range(&self, part: Part, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> KvIter<'_> {
        self.rows(part, self.runs[&part].range(lower, upper))
    }

    fn iter(&self, part: Part) -> KvIter<'_> {
        self.rows(part, Ok(0..self.runs[&part].rows))
    }

    fn callee_keys_matching(&self, _needle: &str, _cursor_after: Option<&[u8]>, lower: Bound<Vec<u8>>) -> KvIter<'_> {
        self.range(Part::CallsByCallee, lower.as_ref().map(Vec::as_slice), Bound::Unbounded)
    }

    fn trait_keys_matching(&self, _needle: &str, _cursor_after: Option<&[u8]>, lower: Bound<Vec<u8>>) -> KvIter<'_> {
        self.range(
            Part::ImplementationsByTrait,
            lower.as_ref().map(Vec::as_slice),
            Bound::Unbounded,
        )
    }
}
