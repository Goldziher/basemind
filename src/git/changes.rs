//! The files two revisions differ in: the tree-to-tree diff a change scope between two revisions
//! reads (`engine::scope`). Kept beside `commit.rs` so the gix boundary stays inside `git`.

use super::commit::{classify_tree_change, decode_path};
use super::{GitError, Repo};
use crate::path::RelPath;

impl Repo {
    /// Every file whose entry differs between the trees of `from` and `to`, sorted and
    /// deduplicated, as `git diff --name-only from to` lists them. Each side is any revision git
    /// can parse and is peeled to its tree, so an annotated tag names its commit's tree. A rename
    /// names both its source and its destination; a directory is never named.
    pub fn changed_paths_between(&self, from: &str, to: &str) -> Result<Vec<RelPath>, GitError> {
        let local = self.local();
        let old = peeled_tree(&local, from)?;
        let new = peeled_tree(&local, to)?;
        let read_error = |msg: String| GitError::Read {
            what: format!("diff {from}..{to}"),
            msg,
        };
        let mut platform = old.changes().map_err(|e| read_error(e.to_string()))?;
        let mut out = Vec::new();
        platform
            .for_each_to_obtain_tree(&new, |change| {
                if let Some((path, _)) = classify_tree_change(&change) {
                    out.push(path);
                }
                if let gix::object::tree::diff::Change::Rewrite {
                    source_location,
                    source_entry_mode,
                    copy: false,
                    ..
                } = &change
                    && !source_entry_mode.is_tree()
                    && let Some(source) = decode_path(source_location)
                {
                    out.push(source);
                }
                Ok(gix::object::tree::diff::Action::Continue(()))
            })
            .map_err(|e| read_error(e.to_string()))?;
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// The tree `rev` names, through any tag or commit on the way.
fn peeled_tree<'r>(local: &'r gix::Repository, rev: &str) -> Result<gix::Tree<'r>, GitError> {
    let id = local.rev_parse_single(rev).map_err(|e| GitError::RevParse {
        rev: rev.to_string(),
        msg: e.to_string(),
    })?;
    let object = id.object().map_err(|e| GitError::Read {
        what: rev.to_string(),
        msg: e.to_string(),
    })?;
    object.peel_to_tree().map_err(|e| GitError::RevParse {
        rev: rev.to_string(),
        msg: format!("does not name a tree: {e}"),
    })
}
