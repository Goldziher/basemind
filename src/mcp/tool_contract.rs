//! The contract every verb tool shares (ADR-0018): how a call is answered, how a failure is
//! reported, and what every advertised tool definition carries.

use std::future::Future;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{CallToolResult, ContentBlock, MetaObject};
use serde_json::Value;

use super::strict::{Strict, ToolArgs};
use super::{BasemindServer, ServerState};
use crate::path::RelPath;

/// Token budget a list response is trimmed to when the caller passes no `max_tokens`. One unbudgeted
/// outline once returned 832 KB; a default bounds every list without the caller having to ask.
pub(super) const DEFAULT_MAX_TOKENS: u32 = 6_000;

/// `_meta` key that keeps a tool's schema loaded in hosts that defer MCP tools and load only names.
pub(crate) const ALWAYS_LOAD_META_KEY: &str = "anthropic/alwaysLoad";

/// The budget to apply: the caller's, or [`DEFAULT_MAX_TOKENS`].
pub(super) fn budget(max_tokens: Option<u32>) -> Option<u32> {
    Some(max_tokens.unwrap_or(DEFAULT_MAX_TOKENS))
}

/// A failure the caller can act on, as a tool result rather than a JSON-RPC error.
pub(super) fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// Report a tool body's error as an `isError` result. JSON-RPC errors are left to the protocol
/// layer (unknown tool, malformed request, cancellation), which never reaches a tool body.
pub(super) fn into_tool_result(result: Result<CallToolResult, McpError>) -> CallToolResult {
    match result {
        Ok(result) => result,
        Err(error) => tool_error(error.message.into_owned()),
    }
}

impl BasemindServer {
    /// Answer one verb-tool call: reject inexact arguments, run the body, and report any failure as
    /// an `isError` result.
    pub(super) async fn answer<T, F, Fut>(&self, args: Strict<T>, body: F) -> Result<CallToolResult, McpError>
    where
        T: ToolArgs,
        F: FnOnce(T) -> Fut,
        Fut: Future<Output = Result<CallToolResult, McpError>>,
    {
        match args.0 {
            Ok(params) => Ok(into_tool_result(body(params).await)),
            Err(message) => Ok(tool_error(message)),
        }
    }
}

/// The error for a caller-supplied path that leaves the repository.
pub(super) fn outside_repository(tool: &str, path: &RelPath) -> McpError {
    McpError::invalid_params(
        format!(
            "`{tool}`: `{path}` is outside this repository. Pass a repo-relative path with no `..`; locate one \
             with `find_files {{\"query\":\"…\"}}`."
        ),
        None,
    )
}

/// Where the files of this workspace's index are read from.
#[derive(Clone, Copy)]
pub(super) struct IndexedFiles<'a> {
    pub root: &'a std::path::Path,
    /// `scan.follow_symlinks` as the scan applied it: the index then holds links whose targets are
    /// outside the root, by the operator's grant.
    follow_symlinks: bool,
    /// `scan.max_file_bytes`: no indexed file is read whole past it.
    pub max_file_bytes: u64,
}

/// Why [`IndexedFiles::file`] did not name a file to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unreadable {
    /// The index holds no such path, so nothing on disk is consulted for it.
    NotIndexed,
    /// The path leaves the repository.
    Outside,
}

impl<'a> IndexedFiles<'a> {
    pub(super) fn of(state: &'a ServerState) -> Self {
        Self {
            root: &state.shared.root,
            follow_symlinks: state.shared.config.scan.follow_symlinks,
            max_file_bytes: state.shared.config.scan.max_file_bytes,
        }
    }

    /// A root whose scan did not follow symlinks.
    #[cfg(test)]
    pub(super) fn contained(root: &'a std::path::Path) -> Self {
        Self {
            root,
            follow_symlinks: false,
            max_file_bytes: u64::MAX,
        }
    }

    /// The working-tree file behind `path`, for reading, when `store` indexes it.
    ///
    /// Membership is looked up here, not promised by the caller: the two relaxations below are
    /// only sound for a path the scan itself admitted. A `scan.extra_roots` file is keyed by its
    /// absolute path and lives outside the repository by design, so its key is read as is. Under
    /// `scan.follow_symlinks` the scan indexed whatever a link resolved to, so only the lexical
    /// check applies and the read follows the link as the scan (and `code_grep`) did. Otherwise
    /// the path is resolved and must stay inside the root, which also covers an indexed file
    /// replaced by a link since the scan.
    pub(super) fn file(&self, store: &crate::store::Store, path: &RelPath) -> Result<std::path::PathBuf, Unreadable> {
        if store.lookup(path).is_none() && store.lookup_doc(path).is_none() {
            return Err(Unreadable::NotIndexed);
        }
        self.indexed_path(path).ok_or(Unreadable::Outside)
    }

    /// [`file`](Self::file) for a path the caller took from the index itself: where to read it,
    /// or `None` when it now leads out of the repository.
    pub(super) fn indexed_path(&self, path: &RelPath) -> Option<std::path::PathBuf> {
        if path.is_external() {
            return Some(path.to_path_buf());
        }
        if self.follow_symlinks {
            crate::path::lexical_file(self.root, path).ok()
        } else {
            crate::path::contained_file(self.root, path).ok()
        }
    }
}

/// [`IndexedFiles::indexed_path`] for a sweep over the whole corpus.
///
/// Resolving every path would cost a `realpath` per file. A file leaves the repository through a
/// link in one of two places: one of its directories, or the file itself. Each distinct directory
/// is resolved once per sweep and remembered, and the file costs one `lstat`.
pub(super) struct Sweep<'a> {
    files: IndexedFiles<'a>,
    /// The resolved root; `None` when it cannot be resolved, and nothing is then inside it.
    root: Option<std::path::PathBuf>,
    /// Directory, as joined to the root → whether it resolves to a place inside the root.
    directories: std::sync::RwLock<ahash::AHashMap<std::path::PathBuf, bool>>,
}

impl<'a> Sweep<'a> {
    pub(super) fn new(files: IndexedFiles<'a>) -> Self {
        Self {
            files,
            root: files.root.canonicalize().ok(),
            directories: std::sync::RwLock::default(),
        }
    }

    /// `scan.max_file_bytes` of the swept index.
    pub(super) fn max_file_bytes(&self) -> u64 {
        self.files.max_file_bytes
    }

    /// Where to read `path`, or `None` when it is gone or now leads out of the repository.
    pub(super) fn path(&self, path: &RelPath) -> Option<std::path::PathBuf> {
        if path.is_external() || self.files.follow_symlinks {
            return self.files.indexed_path(path);
        }
        let joined = crate::path::lexical_file(self.files.root, path).ok()?;
        if !self.directory_is_inside(joined.parent()?) {
            return None;
        }
        match std::fs::symlink_metadata(&joined) {
            Ok(metadata) if metadata.file_type().is_symlink() => self.files.indexed_path(path),
            Ok(_) => Some(joined),
            Err(_) => None,
        }
    }

    fn directory_is_inside(&self, directory: &std::path::Path) -> bool {
        if let Some(inside) = self
            .directories
            .read()
            .ok()
            .and_then(|known| known.get(directory).copied())
        {
            return inside;
        }
        let inside = self.root.as_deref().is_some_and(|root| {
            directory
                .canonicalize()
                .is_ok_and(|resolved| resolved.starts_with(root))
        });
        if let Ok(mut known) = self.directories.write() {
            known.insert(directory.to_path_buf(), inside);
        }
        inside
    }
}

/// [`IndexedFiles::file`] with its refusal phrased for the caller of `tool`.
pub(super) fn indexed_file(
    state: &ServerState,
    store: &crate::store::Store,
    tool: &str,
    path: &RelPath,
) -> Result<std::path::PathBuf, McpError> {
    IndexedFiles::of(state)
        .file(store, path)
        .map_err(|refusal| match refusal {
            Unreadable::NotIndexed => not_indexed_in(store, &state.shared.root, tool, path),
            Unreadable::Outside => outside_repository(tool, path),
        })
}

/// Why `path` cannot be served from the code map, phrased as the next call to make.
///
/// "Not indexed" has three different fixes, and an agent that is only told the first word guesses:
/// a document is searched with `docs_search`, a file that exists needs `index_rescan`, and a path
/// that does not exist needs `find_files`. Judged on the store's file map: the in-RAM map is also
/// empty before a lazy build.
pub(super) fn not_indexed_in(
    store: &crate::store::Store,
    root: &std::path::Path,
    tool: &str,
    path: &RelPath,
) -> McpError {
    not_indexed_because(root, tool, path, index_facts(store, path))
}

/// What the file map says about a path the code map could not serve.
#[derive(Default)]
struct IndexFacts {
    /// The index holds no file at all.
    empty: bool,
    /// The path is an indexed document.
    is_document: bool,
    /// The path is in the file map, yet its extraction blob is absent or cannot be read.
    blob_lost: bool,
}

fn index_facts(store: &crate::store::Store, path: &RelPath) -> IndexFacts {
    let blob_lost = store
        .lookup(path)
        .is_some_and(|entry| !matches!(store.read_l1_by_hex(&entry.hash_hex), Ok(Some(_))));
    IndexFacts {
        empty: store.index.files.is_empty() && store.index.doc_files.is_empty(),
        is_document: store.lookup_doc(path).is_some(),
        blob_lost,
    }
}

fn not_indexed_because(root: &std::path::Path, tool: &str, path: &RelPath, facts: IndexFacts) -> McpError {
    let on_disk = crate::path::contained_file(root, path);
    let message = if facts.empty {
        format!("`{tool}`: {EMPTY_INDEX_FIX}")
    } else if on_disk.is_err() {
        return outside_repository(tool, path);
    } else if facts.is_document {
        format!(
            "`{tool}`: `{path}` is a document, not code, so it has no outline or symbols. Search it with \
             `docs_search`, or read a line span with `code_read`."
        )
    } else if facts.blob_lost {
        format!(
            "`{tool}`: `{path}` is indexed, but its cache entry is missing or damaged. Run \
             `index_rescan {{\"paths\":[\"{path}\"]}}` to rebuild it and retry."
        )
    } else if on_disk.is_ok_and(|file| file.is_file()) {
        format!(
            "`{tool}`: `{path}` exists but is not indexed. Run `index_rescan {{\"paths\":[\"{path}\"]}}` and retry; \
             if it is still missing the file is ignored or not a supported language."
        )
    } else {
        format!("`{tool}`: no file `{path}` in this repository. Locate it with `find_files {{\"query\":\"…\"}}`.")
    };
    McpError::invalid_params(message, None)
}

/// The error for a tool whose retrieval stack is not in this build.
#[cfg_attr(
    all(feature = "code-search", feature = "documents", feature = "memory"),
    allow(dead_code, reason = "only the gated-off arms name a missing feature")
)]
pub(super) fn feature_missing(tool: &str, feature: &str) -> McpError {
    McpError::invalid_request(
        format!(
            "`{tool}` needs the `{feature}` Cargo feature, which is not compiled into this basemind binary. \
             Rebuild with `--features {feature}`; the published release binary includes it."
        ),
        None,
    )
}

/// What to do about an index with no files in it.
pub(crate) const EMPTY_INDEX_FIX: &str = "the index is empty: nothing has been scanned for this workspace. Run \
     `index_rescan {}` (or `basemind scan` in a shell) and retry.";

/// Finish the assembled router: mark every verb tool always-loaded and strip schema keywords that
/// cost bytes on every session without telling a model anything.
///
/// The marker goes on the verb tools only. `agents` is still a mode-dispatched domain tool and
/// stays deferred, so a session that never coordinates with another agent does not pay for it.
/// Applied to the router rather than per
/// `#[tool(meta = …)]` so a verb tool added later cannot forget the marker;
/// `tests/mcp_schema_wire.rs` asserts both halves.
pub(super) fn finalize_router(mut router: ToolRouter<BasemindServer>) -> ToolRouter<BasemindServer> {
    for route in router.map.values_mut() {
        if super::types_tools::VERB_TOOLS.contains(&route.attr.name.as_ref()) {
            let mut meta = route.attr.meta.take().unwrap_or_else(MetaObject::new);
            meta.0.insert(ALWAYS_LOAD_META_KEY.to_string(), Value::Bool(true));
            route.attr.meta = Some(meta);
        }

        let mut schema = Value::Object((*route.attr.input_schema).clone());
        compact_schema(&mut schema, true);
        if let Value::Object(object) = schema {
            route.attr.input_schema = std::sync::Arc::new(object);
        }
    }
    router
}

/// Drop `$schema` / the root `title`, integer `format` / `minimum`, and the `null` alternative of an
/// optional field. Optional fields are already absent from `required`, which is what a model reads.
fn compact_schema(value: &mut Value, root: bool) {
    let Value::Object(object) = value else {
        return;
    };
    if root {
        object.remove("$schema");
        object.remove("title");
    }
    if object.get("type").is_some_and(|kind| kind == "integer") || type_includes(object, "integer") {
        object.remove("format");
        object.remove("minimum");
    }
    if let Some(Value::Array(kinds)) = object.get("type") {
        let concrete: Vec<Value> = kinds.iter().filter(|kind| *kind != "null").cloned().collect();
        if concrete.len() == 1 && kinds.len() == 2 {
            object.insert("type".to_string(), concrete[0].clone());
        }
    }
    if let Some(Value::Array(variants)) = object.get_mut("enum") {
        variants.retain(|variant| !variant.is_null());
    }
    for (key, child) in object.iter_mut() {
        match child {
            Value::Object(children) if key == "properties" => {
                for property in children.values_mut() {
                    compact_schema(property, false);
                }
            }
            Value::Object(_) if key == "items" => compact_schema(child, false),
            _ => {}
        }
    }
}

fn type_includes(object: &serde_json::Map<String, Value>, wanted: &str) -> bool {
    matches!(object.get("type"), Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compacting_keeps_what_a_model_reads_and_drops_the_rest() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "CodeGrepParams",
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string", "description": "Text to find." },
                "limit": { "type": ["integer", "null"], "format": "uint32", "minimum": 0, "description": "Max." },
                "tags": { "type": ["array", "null"], "items": { "type": "string" } },
                "lane": { "type": ["string", "null"], "enum": ["hybrid", null] }
            }
        });
        compact_schema(&mut schema, true);
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string", "description": "Text to find." },
                    "limit": { "type": "integer", "description": "Max." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "lane": { "type": "string", "enum": ["hybrid"] }
                }
            })
        );
    }

    #[test]
    fn an_empty_index_is_fixed_by_a_call_the_tool_accepts() {
        let notice = crate::mcp::types::LifecycleNotice::empty_index();
        for text in [EMPTY_INDEX_FIX, notice.message] {
            assert!(text.contains("`index_rescan {}`"), "{text}");
            assert!(
                !text.contains("`full`") && !text.contains("\"full\""),
                "`index_rescan` takes no such argument: {text}"
            );
        }
    }

    #[test]
    fn a_body_error_becomes_an_is_error_result_carrying_its_message() {
        let result = into_tool_result(Err(McpError::invalid_params("run `index_rescan`", None)));
        assert_eq!(result.is_error, Some(true));
        let text = match result.content.first() {
            Some(ContentBlock::Text(text)) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert_eq!(text, "run `index_rescan`");
    }

    #[cfg(unix)]
    #[test]
    fn a_sweep_reads_nothing_through_a_directory_or_a_file_that_links_out_of_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join("src/deep")).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(root.join("src/deep/a.rs"), b"a").expect("write");
        std::fs::write(outside.join("b.rs"), b"b").expect("write");
        std::fs::write(outside.join("secret.rs"), b"s").expect("write");
        std::os::unix::fs::symlink(&outside, root.join("linked")).expect("link directory");
        std::os::unix::fs::symlink(outside.join("secret.rs"), root.join("src/leaf.rs")).expect("link file");
        std::os::unix::fs::symlink(root.join("src/deep/a.rs"), root.join("src/inside.rs")).expect("link inside");

        let sweep = Sweep::new(IndexedFiles::contained(&root));
        assert_eq!(
            sweep.path(&RelPath::from("src/deep/a.rs")),
            Some(root.join("src/deep/a.rs"))
        );
        assert_eq!(sweep.path(&RelPath::from("linked/b.rs")), None, "a linked directory");
        assert_eq!(sweep.path(&RelPath::from("linked/absent.rs")), None);
        assert_eq!(sweep.path(&RelPath::from("src/leaf.rs")), None, "a linked file");
        assert!(
            sweep.path(&RelPath::from("src/inside.rs")).is_some(),
            "a link that stays inside"
        );
        assert_eq!(
            sweep.path(&RelPath::from("src/gone.rs")),
            None,
            "deleted since the scan"
        );
        assert_eq!(sweep.path(&RelPath::from("gone/a.rs")), None, "its directory is gone");

        let known = sweep.directories.read().expect("lock");
        assert_eq!(known.len(), 4, "each distinct directory is resolved once: {known:?}");
        assert_eq!(known.get(&root.join("linked")), Some(&false));
        assert_eq!(known.get(&root.join("src/deep")), Some(&true));
    }
}
