//! The progress history of a migration (#130): one NDJSON line per check, appended to
//! `<workspace cache dir>/migrations/<name>.ndjson`. It lives in the cache, never in the
//! repository, so a check writes nothing a commit could pick up.
//!
//! Only the first line and the last [`TAIL_BYTES`] are parsed; the run count is a `memchr` newline
//! count (a run is about 70 bytes). A line that does not parse (a torn write, a hand edit) is
//! skipped, not fatal.

use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::engine::types::migration::RunPoint;

/// Directory of the history files under the workspace cache directory.
const HISTORY_DIR: &str = "migrations";
/// Bytes read from the end of a history file for its recent runs.
const TAIL_BYTES: u64 = 64 * 1024;

/// What was recorded before a run.
#[derive(Debug, Default, PartialEq)]
pub struct Recorded {
    /// Runs in the file, counted by its newlines.
    pub(crate) runs: u32,
    pub(crate) first: Option<RunPoint>,
    /// The latest runs, oldest first.
    pub(crate) recent: Vec<RunPoint>,
}

/// The history file of migration `name` under `cache_dir`.
pub(crate) fn file(cache_dir: &Path, name: &str) -> PathBuf {
    cache_dir.join(HISTORY_DIR).join(format!("{name}.ndjson"))
}

/// The runs recorded in `path`, keeping at most `keep` recent ones. A missing file has none.
pub(crate) fn read(path: &Path, keep: usize) -> Recorded {
    let Ok(mut handle) = std::fs::File::open(path) else {
        return Recorded::default();
    };
    let len = handle.metadata().map(|m| m.len()).unwrap_or(0);
    let mut first_line = String::new();
    let _ = std::io::BufReader::new(&handle).read_line(&mut first_line);
    let first = serde_json::from_str::<RunPoint>(first_line.trim_end()).ok();

    let start = len.saturating_sub(TAIL_BYTES);
    let mut tail = Vec::new();
    if handle.seek(SeekFrom::Start(start)).is_ok() {
        let _ = handle.read_to_end(&mut tail);
    }
    let mut lines: &[u8] = &tail;
    if start > 0 {
        // The tail starts mid-line: drop up to the first newline.
        lines = memchr::memchr(b'\n', lines).map_or(&[][..], |at| &lines[at + 1..]);
    }
    let mut recent: Vec<RunPoint> = lines
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<RunPoint>(line).ok())
        .collect();
    let drop = recent.len().saturating_sub(keep);
    recent.drain(..drop);
    Recorded {
        runs: count_lines(path),
        first,
        recent,
    }
}

/// Newlines in `path`: one per recorded run.
fn count_lines(path: &Path) -> u32 {
    let Ok(mut handle) = std::fs::File::open(path) else {
        return 0;
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut count = 0usize;
    while let Ok(read) = handle.read(&mut buf) {
        if read == 0 {
            break;
        }
        count += memchr::memchr_iter(b'\n', &buf[..read]).count();
    }
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Append `run` to `path` as one line, creating the directory. One `write` of a short line on an
/// append-mode handle, so two concurrent checks do not interleave within a line.
pub(crate) fn append(path: &Path, run: &RunPoint) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(run).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut handle = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    handle.write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(at: u64, remaining: u32) -> RunPoint {
        RunPoint {
            at,
            head: Some("abc1234".to_string()),
            remaining,
            expected: 0,
        }
    }

    #[test]
    fn a_missing_history_has_no_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read(&file(dir.path(), "m"), 10), Recorded::default());
    }

    #[test]
    fn runs_append_and_read_back_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "m");
        for (at, remaining) in [(1, 9), (2, 7), (3, 4)] {
            append(&path, &run(at, remaining)).expect("append");
        }
        let recorded = read(&path, 2);
        assert_eq!(recorded.runs, 3);
        assert_eq!(recorded.first, Some(run(1, 9)));
        assert_eq!(recorded.recent, vec![run(2, 7), run(3, 4)]);
    }

    #[test]
    fn a_torn_line_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "m");
        append(&path, &run(1, 9)).expect("append");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(b"{\"at\":2,\"rem\n"))
            .expect("torn");
        append(&path, &run(3, 4)).expect("append");
        let recorded = read(&path, 10);
        assert_eq!(recorded.recent, vec![run(1, 9), run(3, 4)]);
    }

    #[test]
    fn a_long_history_reads_only_its_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "m");
        let runs = 3000u64;
        for at in 0..runs {
            append(&path, &run(at, 5)).expect("append");
        }
        assert!(
            std::fs::metadata(&path).expect("meta").len() > TAIL_BYTES,
            "the history outgrew the tail"
        );
        let recorded = read(&path, 10);
        assert_eq!(recorded.runs, 3000);
        assert_eq!(recorded.first, Some(run(0, 5)));
        assert_eq!(recorded.recent.len(), 10);
        assert_eq!(recorded.recent.last(), Some(&run(runs - 1, 5)));
    }
}
