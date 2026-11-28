//! One file's content diff between two sides, with git's own binary test.

use std::io::Read;
use std::path::Path;

use super::commit::compute_hunks;
use super::{GitError, Hunk, Repo, resolve_tree};

/// One file's content diff between two sides.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    pub old_exists: bool,
    pub new_exists: bool,
    /// Either side holds a NUL byte in its first `BINARY_SNIFF_BYTES`, git's own test for a
    /// binary file. A binary diff carries no hunks.
    pub binary: bool,
    /// Either side is over `DIFF_SIDE_MAX_BYTES`, so the two were not compared. No hunks.
    pub too_large: bool,
    /// A hunk line was not valid UTF-8: its text holds the line with each invalid byte sequence
    /// decoded as U+FFFD.
    pub lossy_text: bool,
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
            lossy_text: false,
        }
    }

    /// Diff the two sides of a file; `None` when it exists on neither.
    fn between(old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> Option<Self> {
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
        let (hunks, lossy_text) = if binary {
            (Vec::new(), false)
        } else {
            compute_hunks(&old, &new)
        };
        Some(Self {
            hunks,
            old_exists,
            new_exists,
            binary,
            too_large: false,
            lossy_text,
        })
    }

    /// [`Self::between`] over two sides read under the ceiling: a side that was too large to load
    /// makes the whole file `too_large`.
    fn of(old: Side, new: Side) -> Option<Self> {
        match (old, new) {
            (Side::Absent, Side::Absent) => None,
            (Side::TooLarge, new) => Some(Self::too_large(true, !matches!(new, Side::Absent))),
            (old, Side::TooLarge) => Some(Self::too_large(!matches!(old, Side::Absent), true)),
            (old, new) => Self::between(old.into_bytes(), new.into_bytes()),
        }
    }
}

/// One side of a diff, loaded only when it is within [`DIFF_SIDE_MAX_BYTES`].
#[derive(Debug, PartialEq, Eq)]
enum Side {
    Absent,
    Bytes(Vec<u8>),
    /// Over the ceiling; its bytes were not kept.
    TooLarge,
}

/// The text git diffs for a submodule entry.
fn gitlink_text(id: &str) -> Vec<u8> {
    format!("Subproject commit {id}\n").into_bytes()
}

impl Side {
    /// The bytes of a side that is a gitlink line, for a submodule that is not checked out.
    fn as_gitlink_text(&self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(bytes) if bytes.starts_with(b"Subproject commit ") => Some(bytes.clone()),
            _ => None,
        }
    }

    fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Absent | Self::TooLarge => None,
        }
    }

    /// Read `source` up to one byte past the ceiling. A file's length is not trusted for this: it
    /// can grow between a `stat` and the read.
    fn read(source: impl Read) -> std::io::Result<Self> {
        let mut bytes = Vec::new();
        source.take(DIFF_SIDE_MAX_BYTES + 1).read_to_end(&mut bytes)?;
        Ok(if over_ceiling(&bytes) {
            Self::TooLarge
        } else {
            Self::Bytes(bytes)
        })
    }

    /// The working-tree file at `on_disk`; a file that cannot be read is absent.
    fn on_disk(on_disk: &Path) -> Self {
        std::fs::File::open(on_disk)
            .and_then(Self::read)
            .unwrap_or(Self::Absent)
    }

    /// A blob whose header gives `size`, loaded with `load` only when it is within the ceiling.
    fn blob<E>(size: u64, load: impl FnOnce() -> Result<Vec<u8>, E>) -> Result<Self, E> {
        if size > DIFF_SIDE_MAX_BYTES {
            return Ok(Self::TooLarge);
        }
        load().map(Self::Bytes)
    }
}

impl Repo {
    /// Content-level hunks for `path` between `rev_old` and `rev_new`. `None` when the path is
    /// absent at both sides; an identical or binary file yields no hunks.
    pub fn diff_file(
        &self,
        rev_old: &str,
        rev_new: &str,
        path: impl AsRef<Path>,
    ) -> Result<Option<FileDiff>, GitError> {
        let path = path.as_ref();
        Ok(FileDiff::of(
            self.diff_side_at_rev(rev_old, path)?,
            self.diff_side_at_rev(rev_new, path)?,
        ))
    }

    /// Content-level hunks for `path` between `rev_old` and the file as it is in the working tree
    /// (staged or not). Same return shape as [`diff_file`](Self::diff_file).
    ///
    /// The working-tree side is read from disk, so a path that leaves the repository (absolute,
    /// `..`, or a symlink pointing outside) is [`GitError::OutsideRepo`], never read. It is compared
    /// in the form git would store for it, as `git diff` compares it: a CRLF checkout of an LF blob
    /// is unchanged, and a clean-filtered file (Git LFS) diffs pointer against pointer. A filter
    /// that fails is [`GitError::Convert`].
    pub fn diff_file_worktree(&self, rev_old: &str, path: impl AsRef<Path>) -> Result<Option<FileDiff>, GitError> {
        let path = path.as_ref();
        let rel = crate::path::RelPath::from(path);
        let outside = || GitError::OutsideRepo(rel.clone());
        let joined = crate::path::lexical_file(self.workdir(), &rel).map_err(|_| outside())?;
        let old = self.diff_side_at_rev(rev_old, path)?;
        // A symlink is diffed as its link text, as git stores it, and never read through. One that
        // leads out of the repository is still refused, as for any other working-tree read.
        if std::fs::symlink_metadata(&joined).is_ok_and(|meta| meta.file_type().is_symlink()) {
            crate::path::contained_file(self.workdir(), &rel).map_err(|_| outside())?;
            let new = std::fs::read_link(&joined)
                .map(|target| Side::Bytes(target.to_string_lossy().into_owned().into_bytes()))
                .unwrap_or(Side::Absent);
            return Ok(FileDiff::of(old, new));
        }
        if joined.is_dir() && self.is_gitlink_at(rev_old, path) {
            let new = self.submodule_side(&joined).or_else(|| old.as_gitlink_text());
            return Ok(FileDiff::of(old, new.map_or(Side::Absent, Side::Bytes)));
        }
        let on_disk = crate::path::contained_file(self.workdir(), &rel).map_err(|_| outside())?;
        Ok(FileDiff::of(old, self.worktree_side(path, &on_disk)?))
    }

    /// `Subproject commit <head>` for the submodule checked out at `dir`; `None` when it is not
    /// checked out.
    fn submodule_side(&self, dir: &Path) -> Option<Vec<u8>> {
        let submodule = gix::open(dir).ok()?;
        let head = submodule.head_id().ok()?.to_string();
        Some(gitlink_text(&head))
    }

    /// Whether `rel` is a submodule (a gitlink) at `rev`.
    pub fn is_gitlink_at(&self, rev: &str, rel: &Path) -> bool {
        let local = self.local();
        resolve_tree(&local, rev)
            .ok()
            .and_then(|tree| tree.lookup_entry_by_path(rel).ok().flatten())
            .is_some_and(|entry| entry.mode().is_commit())
    }

    /// The working-tree file at `on_disk` in the form git stores for the repo-relative `rel`, held
    /// to the same ceiling after conversion as before it.
    fn worktree_side(&self, rel: &Path, on_disk: &Path) -> Result<Side, GitError> {
        match Side::on_disk(on_disk) {
            Side::Bytes(bytes) => self.read_git_form(rel, &bytes, |converted| Side::read(converted)),
            unread => Ok(unread),
        }
    }

    /// Whether `rel` names a directory (a tree) at `rev`. `false` for a revision that cannot be
    /// resolved or a path it does not have.
    pub fn is_directory_at(&self, rev: &str, rel: &Path) -> bool {
        let local = self.local();
        resolve_tree(&local, rev)
            .ok()
            .and_then(|tree| tree.lookup_entry_by_path(rel).ok().flatten())
            .is_some_and(|entry| entry.mode().is_tree())
    }

    /// The blob for `rel` at `rev`. Its size is read from the object header first, so a blob over
    /// the ceiling is never inflated into memory.
    fn diff_side_at_rev(&self, rev: &str, rel: &Path) -> Result<Side, GitError> {
        let local = self.local();
        let read_error = |msg: String| GitError::Read {
            what: format!("{rev}:{}", rel.display()),
            msg,
        };
        let tree = resolve_tree(&local, rev)?;
        let Some(entry) = tree
            .lookup_entry_by_path(rel)
            .map_err(|error| read_error(error.to_string()))?
        else {
            return Ok(Side::Absent);
        };
        // A gitlink's commit lives in the submodule, not here: its side is the line git prints.
        if entry.mode().is_commit() {
            return Ok(Side::Bytes(gitlink_text(&entry.object_id().to_string())));
        }
        let header = local
            .find_header(entry.object_id())
            .map_err(|error| read_error(error.to_string()))?;
        Side::blob(header.size(), || {
            let object = entry.object().map_err(|error| read_error(error.to_string()))?;
            let blob = object
                .try_into_blob()
                .map_err(|error| read_error(format!("not a blob: {error}")))?;
            Ok(blob.data.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::HunkKind;

    #[test]
    fn a_file_absent_on_both_sides_has_no_diff() {
        assert!(FileDiff::between(None, None).is_none());
    }

    #[test]
    fn a_text_change_yields_hunks_and_is_not_binary() {
        let diff = FileDiff::between(Some(b"a\nb\n".to_vec()), Some(b"a\nc\n".to_vec())).expect("diff");
        assert!(!diff.binary && !diff.lossy_text);
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
    fn a_working_tree_file_over_the_ceiling_is_too_large() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.txt");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(DIFF_SIDE_MAX_BYTES + 1).expect("set_len");
        let old = || Side::Bytes(b"old\n".to_vec());
        let diff = FileDiff::of(old(), Side::on_disk(&path)).expect("diff");
        assert!(diff.too_large && diff.hunks.is_empty());
        assert!(diff.old_exists && diff.new_exists);

        std::fs::write(&path, b"new\n").expect("write");
        let small = FileDiff::of(old(), Side::on_disk(&path)).expect("diff");
        assert!(!small.too_large);
        assert_eq!(small.hunks.len(), 1);
        assert_eq!(Side::on_disk(&dir.path().join("absent")), Side::Absent);
        assert!(FileDiff::of(Side::Absent, Side::Absent).is_none());
    }

    /// Hands out bytes for ever and panics once it has been asked for more than the bound allows.
    struct Endless {
        handed_out: u64,
    }

    impl Read for Endless {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            buffer.fill(b'a');
            self.handed_out += buffer.len() as u64;
            assert!(
                self.handed_out <= DIFF_SIDE_MAX_BYTES + 1,
                "read {} bytes of a side capped at {DIFF_SIDE_MAX_BYTES}",
                self.handed_out
            );
            Ok(buffer.len())
        }
    }

    #[test]
    fn a_side_is_read_no_further_than_one_byte_past_the_ceiling() {
        assert_eq!(Side::read(Endless { handed_out: 0 }).expect("read"), Side::TooLarge);
        let at_the_ceiling = vec![b'a'; DIFF_SIDE_MAX_BYTES as usize];
        assert_eq!(
            Side::read(at_the_ceiling.as_slice()).expect("read"),
            Side::Bytes(at_the_ceiling)
        );
    }

    #[test]
    fn a_blob_over_the_ceiling_is_not_loaded() {
        let never = || -> Result<Vec<u8>, ()> { panic!("an oversized blob was loaded") };
        assert_eq!(Side::blob(DIFF_SIDE_MAX_BYTES + 1, never), Ok(Side::TooLarge));
        let small = Side::blob(DIFF_SIDE_MAX_BYTES, || Ok::<_, ()>(b"small\n".to_vec()));
        assert_eq!(small, Ok(Side::Bytes(b"small\n".to_vec())));
        let removed = FileDiff::of(Side::TooLarge, Side::Absent).expect("diff");
        assert!(removed.too_large && removed.old_exists && !removed.new_exists);
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = crate::testing::git_command()
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.test")
            .args(args)
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).expect("utf-8").trim().to_string()
    }

    /// The oversized blob's loose object is cut short after it is committed: its header still
    /// inflates, its body does not. Only a diff that stops at the header can answer.
    #[test]
    fn a_committed_blob_over_the_ceiling_is_sized_from_its_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("huge.txt"), b"small\n").expect("write");
        git(root, &["add", "huge.txt"]);
        git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "small"]);
        std::fs::write(root.join("huge.txt"), vec![b'a'; DIFF_SIDE_MAX_BYTES as usize + 1]).expect("write");
        git(root, &["add", "huge.txt"]);
        git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "huge"]);

        let oid = git(root, &["rev-parse", "HEAD:huge.txt"]);
        let object = root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        let stored = std::fs::read(&object).expect("read object");
        std::fs::remove_file(&object).expect("remove object");
        std::fs::write(&object, &stored[..stored.len() / 2]).expect("truncate object");

        let repo = Repo::discover(root).expect("repo");
        let grown = repo
            .diff_file("HEAD~1", "HEAD", "huge.txt")
            .expect("diff")
            .expect("present");
        assert!(grown.too_large && grown.hunks.is_empty() && grown.old_exists && grown.new_exists);

        std::fs::write(root.join("huge.txt"), b"small again\n").expect("write");
        let shrunk = repo
            .diff_file_worktree("HEAD", "huge.txt")
            .expect("diff")
            .expect("present");
        assert!(shrunk.too_large && shrunk.old_exists && shrunk.new_exists);

        let small = repo
            .diff_file_worktree("HEAD~1", "huge.txt")
            .expect("diff")
            .expect("present");
        assert!(!small.too_large);
        assert_eq!(small.hunks.len(), 1);
    }

    /// Each hunk as its kind, line numbers and text.
    fn hunk_rows(diff: &FileDiff) -> Vec<(HunkKind, u32, u32, u32, u32, &str)> {
        diff.hunks
            .iter()
            .map(|hunk| {
                (
                    hunk.kind,
                    hunk.old_line_start,
                    hunk.old_line_count,
                    hunk.new_line_start,
                    hunk.new_line_count,
                    hunk.text.as_str(),
                )
            })
            .collect()
    }

    /// A Latin-1 file: `\xef` (ï), `\xe9` (é) and `\xe7` (ç) are not UTF-8 on their own.
    #[test]
    fn a_line_that_is_not_utf8_is_kept_in_its_hunk_and_marks_the_diff_lossy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("latin1.txt"), b"first\nna\xefve\nlast\n").expect("write");
        git(root, &["add", "latin1.txt"]);
        git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "latin-1"]);
        std::fs::write(root.join("latin1.txt"), b"first\nna\xefve caf\xe9\nlast\n").expect("write");
        git(root, &["add", "latin1.txt"]);
        git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "changed"]);

        let repo = Repo::discover(root).expect("repo");
        let changed = repo
            .diff_file("HEAD~1", "HEAD", "latin1.txt")
            .expect("diff")
            .expect("present");
        assert!(changed.lossy_text && !changed.binary);
        assert_eq!(
            hunk_rows(&changed),
            [(
                HunkKind::Modified,
                2,
                1,
                2,
                1,
                "-na\u{FFFD}ve\n+na\u{FFFD}ve caf\u{FFFD}\n"
            )]
        );

        std::fs::write(root.join("latin1.txt"), b"first\nna\xefve caf\xe9\nlast\nfa\xe7ade\n").expect("write");
        let appended = repo
            .diff_file_worktree("HEAD", "latin1.txt")
            .expect("diff")
            .expect("present");
        assert!(appended.lossy_text);
        assert_eq!(hunk_rows(&appended), [(HunkKind::Added, 4, 0, 4, 1, "fa\u{FFFD}ade\n")]);

        std::fs::write(root.join("latin1.txt"), b"first\nna\xefve caf\xe9\nlast\nfacade\n").expect("write");
        let utf8_only = repo
            .diff_file_worktree("HEAD", "latin1.txt")
            .expect("diff")
            .expect("present");
        assert!(!utf8_only.lossy_text, "the only hunk line is UTF-8");
        assert_eq!(hunk_rows(&utf8_only), [(HunkKind::Added, 4, 0, 4, 1, "facade\n")]);
    }
}
