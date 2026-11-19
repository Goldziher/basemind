//! The `changed` argument of `code_grep`, `code_symbols`, `code_refs` (call sites) and `find_files`
//! (#122): a [`ChangeSpec`] resolved to the files it touches, which each op applies as one more path
//! predicate.
//!
//! The scope is resolved before an op takes the map cache or the store lock, because the importer
//! expansion builds the import graph under the store read lock. A `+importers` suffix adds the files
//! importing a changed file directly (depth 1, runtime imports: [`Tiers::RUNTIME`]); a changed file
//! in a language no resolver reads gets no importers.

use super::EngineError;
use super::import_graph::{Tiers, import_graph};
use super::scope::{ChangeScope, ChangeSpec, change_scope};
use super::workspace::Workspace;
use crate::path::RelPath;

/// The scope `changed` names for `tool`, or `None` when the caller passed none. An unknown revision,
/// a malformed value, a workspace that is not a repository's root, and `+importers` where no indexed
/// language has a resolver are each refused with the fix.
pub(crate) fn resolve(ws: &Workspace, tool: &str, changed: Option<&str>) -> Result<Option<ChangeScope>, EngineError> {
    let Some(changed) = changed else {
        return Ok(None);
    };
    let named = |error: EngineError| match error {
        EngineError::InvalidArgument(message) => {
            EngineError::invalid_argument(format!("`{tool}`: `changed`: {message}"))
        }
        other => other,
    };
    let spec = ChangeSpec::parse(changed).map_err(named)?;
    let repo = ws
        .repo
        .as_ref()
        .filter(|repo| repo.origin_is_workdir())
        .ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "`{tool}`: `changed` needs the workspace to be the root of a git repository; drop `changed` to \
             search every file."
            ))
        })?;
    let mut scope = change_scope(repo, &spec.base).map_err(named)?;
    if spec.importers && !scope.is_empty() {
        let graph = import_graph(ws)?;
        if !graph.coverage().any_resolvable() {
            return Err(EngineError::invalid_argument(format!(
                "`{tool}`: `changed`: `+importers` needs import resolution, and no indexed language has a \
                 resolver in this build; drop `+importers`."
            )));
        }
        let importers: Vec<RelPath> = scope
            .paths()
            .iter()
            .filter_map(|path| graph.node(path.as_str()?))
            .flat_map(|node| graph.importers(node, Tiers::RUNTIME))
            .map(|edge| graph.path(edge.node).clone())
            .collect();
        scope.extend(importers);
    }
    Ok(Some(scope))
}

/// Whether `path` passes the scope: every path does when there is none.
pub(crate) fn admits(scope: Option<&ChangeScope>, path: &RelPath) -> bool {
    scope.is_none_or(|scope| scope.contains(path))
}
