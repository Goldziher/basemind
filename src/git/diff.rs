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
}

/// How far into a blob the binary check looks, as git does.
const BINARY_SNIFF_BYTES: usize = 8_000;

fn looks_binary(bytes: &[u8]) -> bool {
    memchr::memchr(0, &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)]).is_some()
}

impl FileDiff {
    /// Diff the two sides of a file; `None` when it exists on neither.
    pub(super) fn between(old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> Option<Self> {
        if old.is_none() && new.is_none() {
            return None;
        }
        let old_exists = old.is_some();
        let new_exists = new.is_some();
        let old = old.unwrap_or_default();
        let new = new.unwrap_or_default();
        let binary = looks_binary(&old) || looks_binary(&new);
        let hunks = if binary { Vec::new() } else { compute_hunks(&old, &new) };
        Some(Self {
            hunks,
            old_exists,
            new_exists,
            binary,
        })
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
}
