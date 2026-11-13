use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use memmap2::Mmap;
use tempfile::tempdir;

use super::IndexDb;
use super::view::{IndexView, Part, StandaloneView};

const MAGIC: &[u8; 8] = b"BMSRUN01";
const OFFSET_BYTES: usize = 8;
const LENGTH_BYTES: usize = 4;

fn invalid_run() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid sorted run")
}

fn write_run(
    path: &Path,
    rows: impl Iterator<Item = io::Result<(impl AsRef<[u8]>, impl AsRef<[u8]>)>>,
) -> io::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    writer.write_all(MAGIC)?;
    let mut offsets = Vec::new();
    let mut offset = MAGIC.len() as u64;
    for row in rows {
        let (key, value) = row?;
        let (key, value) = (key.as_ref(), value.as_ref());
        let key_len = u32::try_from(key.len()).map_err(|_| invalid_run())?;
        offsets.push(offset);
        writer.write_all(&key_len.to_le_bytes())?;
        writer.write_all(key)?;
        writer.write_all(value)?;
        offset += LENGTH_BYTES as u64 + key.len() as u64 + value.len() as u64;
    }
    offsets.push(offset);
    for position in offsets {
        writer.write_all(&position.to_le_bytes())?;
    }
    writer.write_all(&offset.to_le_bytes())?;
    writer.flush()
}

struct SortedRun {
    data: Mmap,
    table: usize,
    rows: usize,
}

impl SortedRun {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: Prototype runs are created in a private temporary directory, closed by
        // the writer before mapping and never modified while any reader holds a mapping.
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
        Ok(Self { data, table, rows })
    }

    fn row(&self, index: usize) -> io::Result<(&[u8], &[u8])> {
        if index >= self.rows {
            return Err(invalid_run());
        }
        let slot = self.table + index * OFFSET_BYTES;
        let start = read_offset(&self.data, slot).ok_or_else(invalid_run)?;
        let end = read_offset(&self.data, slot + OFFSET_BYTES).ok_or_else(invalid_run)?;
        if start < MAGIC.len() || end > self.table {
            return Err(invalid_run());
        }
        let row = self.data.get(start..end).ok_or_else(invalid_run)?;
        let length = row.get(..LENGTH_BYTES).ok_or_else(invalid_run)?;
        let key_len = u32::from_le_bytes(length.try_into().map_err(|_| invalid_run())?) as usize;
        row[LENGTH_BYTES..].split_at_checked(key_len).ok_or_else(invalid_run)
    }

    fn lower_bound(&self, key: &[u8]) -> io::Result<usize> {
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

#[test]
fn sorted_run_reads_binary_keys_and_empty_values() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("run");
    let rows: &[(&[u8], &[u8])] = &[(b"a", b""), (b"a\0", b"value"), (b"\xff", b"last")];
    write_run(&path, rows.iter().copied().map(Ok)).unwrap();
    let first = SortedRun::open(&path).unwrap();
    let second = SortedRun::open(&path).unwrap();
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(first.row(index).unwrap(), *row);
        assert_eq!(second.lower_bound(row.0).unwrap(), index);
    }
    assert_eq!(first.lower_bound(b"b").unwrap(), 2);
    assert_eq!(first.lower_bound(b"\xff\xff").unwrap(), rows.len());
    assert!(first.row(rows.len()).is_err());
}

#[test]
fn sorted_run_handles_empty_and_rejects_truncated_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("run");
    write_run(&path, std::iter::empty::<io::Result<(&[u8], &[u8])>>()).unwrap();
    {
        let run = SortedRun::open(&path).unwrap();
        assert_eq!(run.lower_bound(b"key").unwrap(), 0);
    }
    std::fs::write(&path, MAGIC).unwrap();
    assert!(SortedRun::open(&path).is_err());
}

#[test]
fn sorted_runs_match_a_scanned_index() {
    let source = tempdir().unwrap();
    let store = super::view_tests::scanned_store(source.path());
    let db = store.index_db.as_ref().unwrap();
    assert!(compare_parts(&db.view()) > 0);
}

#[test]
#[ignore = "requires a disposable, complete Fjall view from a reference scan"]
fn compare_sorted_run_with_fjall() {
    let path = std::env::var_os("BASEMIND_PROTOTYPE_VIEW").expect("set BASEMIND_PROTOTYPE_VIEW");
    let started = Instant::now();
    let db = IndexDb::open(Path::new(&path)).unwrap();
    eprintln!("Fjall open: {:?}", started.elapsed());
    assert!(compare_parts(&StandaloneView(&db)) > 0);
}

fn compare_parts(view: &impl IndexView) -> usize {
    let output = tempdir().unwrap();
    let mut total = 0;
    for part in Part::ALL {
        let path = output.path().join(part.name());
        let started = Instant::now();
        let rows = view.iter(part).map(|row| row.map_err(io::Error::other));
        write_run(&path, rows).unwrap();
        let exported = started.elapsed();
        let started = Instant::now();
        let run = SortedRun::open(&path).unwrap();
        let mapped = started.elapsed();
        let mut count = 0;
        for row in view.iter(part) {
            let (key, value) = row.unwrap();
            assert_eq!(run.row(count).unwrap(), (key.as_ref(), value.as_ref()));
            assert_eq!(run.lower_bound(&key).unwrap(), count);
            count += 1;
        }
        assert_eq!(run.rows, count);
        total += count;
        eprintln!(
            "{}: {} rows, {} bytes, export {:?}, map {:?}",
            part.name(),
            count,
            run.data.len(),
            exported,
            mapped
        );
    }
    total
}
