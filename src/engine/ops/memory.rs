//! `memory_save`, `memory_search` and `basemind memory delete` (no MCP tool).
//!
//! The tools are advertised in every build and gate here: an agent looking for "remember this" is
//! better served a tool that answers "rebuild with `--features memory`" than a tool that is not
//! there, and never an empty result it would read as "nothing stored".

use crate::engine::error::EngineError;
use crate::engine::types::memory::{MemoryDeleteParams, MemorySaveParams, MemorySearchToolParams};
use crate::engine::workspace::Workspace;

#[cfg(not(feature = "memory"))]
impl Workspace {
    /// `memory_save` in a build without the memory store.
    pub(crate) fn memory_save(&self, _p: MemorySaveParams) -> Result<(), EngineError> {
        Err(EngineError::feature_missing("memory_save", "memory"))
    }

    /// `memory_search` in a build without the memory store.
    pub(crate) fn memory_search(&self, _p: MemorySearchToolParams) -> Result<(), EngineError> {
        Err(EngineError::feature_missing("memory_search", "memory"))
    }

    /// `basemind memory delete` in a build without the memory store.
    pub(crate) fn memory_delete(&self, _p: MemoryDeleteParams) -> Result<(), EngineError> {
        Err(EngineError::feature_missing("memory delete", "memory"))
    }
}

#[cfg(feature = "memory")]
mod enabled {
    use super::*;
    use crate::engine::types::memory::{
        MemoryDeleteResponse, MemoryGetParams, MemoryListParams, MemoryMissing, MemoryPutParams, MemoryPutResponse,
        MemorySearchAnswer, MemorySearchParams,
    };

    /// The argument shapes `memory_search` accepts, quoted in every shape error.
    const SEARCH_SHAPES: &str = "pass at most one of `query` (search by meaning), `key` (read one note) or `prefix` \
         (list notes by key prefix); with none of them every note is listed. `tags` filters a search or a listing; \
         `cursor` pages a listing";

    impl Workspace {
        /// `memory_save`: write a durable note.
        pub(crate) fn memory_save(&self, p: MemorySaveParams) -> Result<MemoryPutResponse, EngineError> {
            self.memory_put(MemoryPutParams {
                key: p.key,
                value: p.value,
                tags: p.tags,
                embed: true,
                visibility: p.visibility.unwrap_or_default(),
            })
        }

        /// `memory_search`: read one note, list notes, or search them by meaning.
        pub(crate) fn memory_search(&self, p: MemorySearchToolParams) -> Result<MemorySearchAnswer, EngineError> {
            let shape_error =
                |problem: &str| EngineError::invalid_argument(format!("`memory_search`: {problem}; {SEARCH_SHAPES}"));
            let visibility = p.visibility.unwrap_or_default();
            match (p.query, p.key, p.prefix) {
                (Some(query), None, None) => {
                    if p.cursor.is_some() {
                        return Err(shape_error(
                            "a `query` search is ranked and takes no `cursor`; raise `limit`",
                        ));
                    }
                    let params = MemorySearchParams {
                        query,
                        limit: p.limit,
                        tags: p.tags.unwrap_or_default(),
                        visibility,
                    };
                    crate::engine::memory::vectors::search(self, params).map(MemorySearchAnswer::Search)
                }
                (None, Some(key), None) => {
                    if p.tags.is_some() || p.limit.is_some() || p.cursor.is_some() {
                        return Err(shape_error(
                            "a `key` read returns one note and takes no `tags`, `limit` or `cursor`",
                        ));
                    }
                    let missing = MemoryMissing {
                        key: key.clone(),
                        found: false,
                    };
                    Ok(self
                        .memory_get(MemoryGetParams { key, visibility })?
                        .map_or(MemorySearchAnswer::Missing(missing), MemorySearchAnswer::Get))
                }
                (None, None, prefix) => self
                    .memory_list(MemoryListParams {
                        prefix,
                        tags: p.tags.unwrap_or_default(),
                        limit: p.limit,
                        cursor: p.cursor,
                        visibility,
                    })
                    .map(MemorySearchAnswer::List),
                _ => Err(shape_error("`query`, `key` and `prefix` are alternatives")),
            }
        }

        /// `basemind memory delete`: delete one note. No MCP tool.
        pub(crate) fn memory_delete(&self, p: MemoryDeleteParams) -> Result<MemoryDeleteResponse, EngineError> {
            self.memory_delete_note(p)
        }
    }
}
