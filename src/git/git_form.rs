//! A working-tree file in the form git stores for it, so it compares with a blob the way git
//! compares them: line endings normalized by `core.autocrlf`, `core.eol` and the `.gitattributes`
//! `text` and `eol` attributes, `ident` collapsed, and clean filter drivers such as Git LFS run.

use std::io::Read;
use std::path::Path;

use super::{GitError, Repo};

impl Repo {
    /// `bytes`, the working-tree content of the repo-relative `rel`, in the form git would store
    /// for it.
    ///
    /// # Errors
    /// [`GitError::Convert`] when the filter pipeline cannot be set up or a filter fails.
    pub fn to_git_form(&self, rel: &Path, bytes: &[u8]) -> Result<Vec<u8>, GitError> {
        self.read_git_form(rel, bytes, |converted| {
            let mut out = Vec::new();
            converted.read_to_end(&mut out)?;
            Ok(out)
        })
    }

    /// Run `bytes`, the working-tree content of `rel`, through git's to-git filter pipeline and
    /// hand the converted stream to `consume`, which may stop reading early.
    pub(super) fn read_git_form<T>(
        &self,
        rel: &Path,
        bytes: &[u8],
        consume: impl FnOnce(&mut dyn Read) -> std::io::Result<T>,
    ) -> Result<T, GitError> {
        let failed = |msg: String| GitError::Convert {
            path: crate::path::RelPath::from(rel),
            msg,
        };
        let local = self.local();
        let (mut pipeline, index) = local.filter_pipeline(None).map_err(|error| failed(error.to_string()))?;
        let mut converted = pipeline
            .convert_to_git(bytes, rel, &index)
            .map_err(|error| failed(error.to_string()))?;
        consume(&mut converted).map_err(|error| failed(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::git::{GitError, HunkKind, Repo};

    /// A hunk as its kind, old start and count, new start and count, and text.
    type Row = (HunkKind, u32, u32, u32, u32, String);

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.test")
            .args(args)
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    /// A repository with `config` set in its own config and `files` committed, written as given.
    fn committed(config: &[(&str, &str)], files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        for &(key, value) in config {
            git(root, &["config", key, value]);
        }
        for &(name, content) in files {
            std::fs::write(root.join(name), content).expect("write");
            git(root, &["add", name]);
        }
        git(root, &["commit", "-qm", "init"]);
        dir
    }

    /// Each hunk of the working-tree diff of `path` against HEAD as its kind, line numbers and text.
    fn worktree_hunks(root: &Path, path: &str) -> Vec<Row> {
        let repo = Repo::discover(root).expect("repo");
        let diff = repo.diff_file_worktree("HEAD", path).expect("diff").expect("present");
        diff.hunks
            .into_iter()
            .map(|hunk| {
                (
                    hunk.kind,
                    hunk.old_line_start,
                    hunk.old_line_count,
                    hunk.new_line_start,
                    hunk.new_line_count,
                    hunk.text,
                )
            })
            .collect()
    }

    fn one_edited_line(old: &str, new: &str) -> Vec<Row> {
        vec![(HunkKind::Modified, 2, 1, 2, 1, format!("-{old}\n+{new}\n"))]
    }

    #[test]
    fn a_crlf_checkout_under_autocrlf_diffs_only_its_edited_lines() {
        let dir = committed(&[("core.autocrlf", "true")], &[("a.txt", "one\ntwo\nthree\n")]);
        let root = dir.path();
        std::fs::write(root.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").expect("write");
        assert_eq!(worktree_hunks(root, "a.txt"), Vec::<Row>::new());

        std::fs::write(root.join("a.txt"), b"one\r\nTWO\r\nthree\r\n").expect("write");
        assert_eq!(worktree_hunks(root, "a.txt"), one_edited_line("two", "TWO"));
    }

    #[test]
    fn a_crlf_checkout_under_an_eol_attribute_diffs_only_its_edited_lines() {
        let dir = committed(
            &[],
            &[
                (".gitattributes", "*.txt text eol=crlf\n"),
                ("a.txt", "one\ntwo\nthree\n"),
            ],
        );
        let root = dir.path();
        std::fs::write(root.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").expect("write");
        assert_eq!(worktree_hunks(root, "a.txt"), Vec::<Row>::new());

        std::fs::write(root.join("a.txt"), b"one\r\nTWO\r\nthree\r\n").expect("write");
        assert_eq!(worktree_hunks(root, "a.txt"), one_edited_line("two", "TWO"));
    }

    /// The fake driver stands in for Git LFS: git stores `pointer` lines where the working tree
    /// holds `payload` lines.
    #[test]
    fn a_clean_filtered_file_diffs_pointer_against_pointer() {
        let dir = committed(
            &[("filter.fake.clean", "sed s/payload/pointer/")],
            &[
                (".gitattributes", "*.bin filter=fake\n"),
                ("data.bin", "payload head\npayload 1\npayload tail\n"),
            ],
        );
        let root = dir.path();
        let repo = Repo::discover(root).expect("repo");
        assert_eq!(
            repo.read_blob_at_rev("HEAD", "data.bin")
                .expect("read")
                .expect("present"),
            b"pointer head\npointer 1\npointer tail\n"
        );
        assert_eq!(worktree_hunks(root, "data.bin"), Vec::<Row>::new());

        std::fs::write(root.join("data.bin"), b"payload head\npayload 2\npayload tail\n").expect("write");
        assert_eq!(
            worktree_hunks(root, "data.bin"),
            one_edited_line("pointer 1", "pointer 2")
        );
    }

    #[test]
    fn a_required_filter_that_fails_is_an_error_not_a_whole_file_hunk() {
        let dir = committed(&[], &[("data.bin", "payload\n")]);
        let root = dir.path();
        git(root, &["config", "filter.broken.clean", "false"]);
        git(root, &["config", "filter.broken.required", "true"]);
        std::fs::write(root.join(".gitattributes"), b"*.bin filter=broken\n").expect("write");

        let repo = Repo::discover(root).expect("repo");
        let error = repo
            .diff_file_worktree("HEAD", "data.bin")
            .expect_err("conversion fails");
        assert!(
            matches!(&error, GitError::Convert { path, .. } if path.as_bytes() == b"data.bin"),
            "{error:?}"
        );
    }
}
