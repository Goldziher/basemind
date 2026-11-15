//! One file's content diff between two sides, with git's own binary test.

use super::Hunk;
use super::commit::compute_hunks;

/// One file's content diff between two sides.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    pub old_exists: bool,
    pub new_exists: bool,
    /// Either side holds a NUL byte in its first [`BINARY_SNIFF_BYTES`], git's own test for a
    /// binary file. A binary diff carries no hunks.
    pub binary: bool,
    /// Either side is over [`DIFF_SIDE_MAX_BYTES`], so the two were not compared. No hunks.
    pub too_large: bool,
}

/// How far into a blob the binary check looks, as git does.
const BINARY_SNIFF_BYTES: usize = 8_000;

/// Largest side a text diff is computed for. The diff holds both sides and its hunk text in memory
/// at once, and a file this size is generated output whose hunks nobody reads.
pub const DIFF_SIDE_MAX_BYTES: u64 = 4 * 1024 * 1024;

fn looks_binary(bytes: &[u8]) -> bool {
    memchr::memchr(0, &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)]).is_some()
}

fn over_ceiling(bytes: &[u8]) -> bool {
    bytes.len() as u64 > DIFF_SIDE_MAX_BYTES
}

impl FileDiff {
    /// The answer for a file that was not compared because a side is over the ceiling.
    fn too_large(old_exists: bool, new_exists: bool) -> Self {
        Self {
            hunks: Vec::new(),
            old_exists,
            new_exists,
            binary: false,
            too_large: true,
        }
    }

    /// Diff the two sides of a file; `None` when it exists on neither.
    pub(super) fn between(old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> Option<Self> {
        if old.is_none() && new.is_none() {
            return None;
        }
        let old_exists = old.is_some();
        let new_exists = new.is_some();
        let old = old.unwrap_or_default();
        let new = new.unwrap_or_default();
        if over_ceiling(&old) || over_ceiling(&new) {
            return Some(Self::too_large(old_exists, new_exists));
        }
        let binary = looks_binary(&old) || looks_binary(&new);
        let hunks = if binary { Vec::new() } else { compute_hunks(&old, &new) };
        Some(Self {
            hunks,
            old_exists,
            new_exists,
            binary,
            too_large: false,
        })
    }

    /// [`Self::between`] with the new side read from `on_disk`. The file's size is checked before
    /// it is read, so a file over the ceiling costs a `stat`, not its length in memory.
    pub(super) fn against_file(old: Option<Vec<u8>>, on_disk: &std::path::Path) -> Option<Self> {
        if std::fs::metadata(on_disk).is_ok_and(|meta| meta.is_file() && meta.len() > DIFF_SIDE_MAX_BYTES) {
            return Some(Self::too_large(old.is_some(), true));
        }
        Self::between(old, std::fs::read(on_disk).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_absent_on_both_sides_has_no_diff() {
        assert!(FileDiff::between(None, None).is_none());
    }

    #[test]
    fn a_text_change_yields_hunks_and_is_not_binary() {
        let diff = FileDiff::between(Some(b"a\nb\n".to_vec()), Some(b"a\nc\n".to_vec())).expect("diff");
        assert!(!diff.binary);
        assert_eq!(diff.hunks.len(), 1);
        assert!(diff.old_exists && diff.new_exists);
    }

    #[test]
    fn a_nul_byte_on_either_side_makes_the_diff_binary_with_no_hunks() {
        let diff = FileDiff::between(Some(b"text\n".to_vec()), Some(vec![0, 1, 2])).expect("diff");
        assert!(diff.binary);
        assert!(diff.hunks.is_empty());
        let added = FileDiff::between(None, Some(vec![b'a', 0])).expect("diff");
        assert!(added.binary && !added.old_exists && added.new_exists);
    }

    #[test]
    fn a_side_over_the_ceiling_is_reported_too_large_and_not_diffed() {
        let huge = vec![b'a'; DIFF_SIDE_MAX_BYTES as usize + 1];
        let added = FileDiff::between(None, Some(huge.clone())).expect("diff");
        assert!(added.too_large && added.hunks.is_empty() && !added.binary);
        assert!(!added.old_exists && added.new_exists);
        let removed = FileDiff::between(Some(huge), Some(b"small\n".to_vec())).expect("diff");
        assert!(removed.too_large && removed.old_exists && removed.new_exists);

        let at_the_ceiling = vec![b'a'; DIFF_SIDE_MAX_BYTES as usize];
        let diffed = FileDiff::between(None, Some(at_the_ceiling)).expect("diff");
        assert!(!diffed.too_large);
        assert_eq!(diffed.hunks.len(), 1);
    }

    #[test]
    fn a_working_tree_file_over_the_ceiling_is_not_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.txt");
        let file = std::fs::File::create(&path).expect("create");
        // Sparse: the length is all the check may look at.
        file.set_len(DIFF_SIDE_MAX_BYTES + 1).expect("set_len");
        let diff = FileDiff::against_file(Some(b"old\n".to_vec()), &path).expect("diff");
        assert!(diff.too_large && diff.hunks.is_empty());
        assert!(diff.old_exists && diff.new_exists);

        std::fs::write(&path, b"new\n").expect("write");
        let small = FileDiff::against_file(Some(b"old\n".to_vec()), &path).expect("diff");
        assert!(!small.too_large);
        assert_eq!(small.hunks.len(), 1);
        assert!(FileDiff::against_file(None, &dir.path().join("absent")).is_none());
    }
}
