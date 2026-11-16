//! Binds a pagination cursor to the query that issued it.
//!
//! A cursor is an offset (or a resume key) plus, at most, the generation of the index it was cut
//! from. Nothing in it says WHICH list it is an offset into, so a `code_outline` cursor for one file
//! was accepted by `code_outline` for another, a `code_search` cursor by `docs_search`, a grep
//! cursor by a different pattern: each returned a page of the wrong list with no sign of it.
//!
//! [`bound`] wraps every paginated tool body. It hashes the tool name and the arguments that select
//! the list (everything but `limit`, `cursor` and `max_tokens`), prefixes the body's `next_cursor`
//! with that hash, and on the way in refuses a cursor whose prefix is not this query's. It also
//! makes the stale case uniform: a body that reports `cursor_invalidated` (the index or HEAD moved
//! since the cursor was issued) is turned into the same kind of error as a foreign cursor, where
//! some bodies used to return an empty page that read as "no more results".

use std::future::Future;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::Value;

use super::cursor::Cursor;
use super::strict::ToolArgs;

/// Arguments that page a list without changing which list it is.
const PAGING_KEYS: [&str; 3] = ["limit", "cursor", "max_tokens"];

/// Separates the query hash from the tool's own cursor. Not in the base64url alphabet.
const SEPARATOR: char = '.';

/// The arguments of a paginated verb tool.
pub(super) trait Paged: ToolArgs + Serialize {
    /// `(field, value)` for each string argument whose omission the tool reads as that value.
    /// Passing the default explicitly selects the same list as leaving it out, so the two must
    /// bind a cursor alike. Each entry mirrors the default applied in the tool's body.
    const DEFAULTS: &'static [(&'static str, &'static str)] = &[];

    /// The `cursor` argument.
    fn cursor_mut(&mut self) -> &mut Option<Cursor>;

    /// The `limit` argument.
    fn limit(&self) -> Option<u32>;
}

macro_rules! paged {
    ($($params:ty $(=> [$(($field:literal, $default:literal)),+ $(,)?])?),+ $(,)?) => {
        $(impl Paged for $params {
            $(const DEFAULTS: &'static [(&'static str, &'static str)] = &[$(($field, $default)),+];)?

            fn cursor_mut(&mut self) -> &mut Option<Cursor> {
                &mut self.cursor
            }

            fn limit(&self) -> Option<u32> {
                self.limit
            }
        })+
    };
}

paged!(
    super::types_tools::CodeOutlineParams,
    super::types_tools::CodeSymbolsParams,
    super::types_tools::CodeGrepParams,
    super::types_tools::CodeSearchParams => [("lane", "hybrid")],
    super::types_tools::CodeRefsParams => [("relation", "callers"), ("match", "exact")],
    super::types_tools::FindFilesToolParams,
    super::types_tools::DocsSearchParams,
    super::types_tools::GitLogParams,
    super::types_tools::GitBlameParams => [("rev", "HEAD")],
    super::types_tools::MemorySearchToolParams => [("visibility", "group")],
);

/// The error for a cursor that outlived the index or history it was cut from.
pub(super) fn stale_cursor(tool: &str) -> McpError {
    McpError::invalid_params(
        format!(
            "`{tool}`: the index or git history changed since this cursor was issued, so it no longer points \
             into the list. Call again without `cursor`."
        ),
        None,
    )
}

/// The refusal for a cursor of a paged git walk issued on the other source of commits: the live walk
/// before the git-history index came up, or the index before it fell back to the live walk. The two
/// order commits that a sync appended differently, so the offset may skip or repeat commits.
pub(super) fn walk_switched(tool: &str) -> McpError {
    McpError::invalid_params(
        format!(
            "`{tool}`: the git-history index came up (or stopped answering) since this cursor was issued, and \
             it orders commits differently from the live walk, so the cursor no longer points into the list. \
             Restart the walk: call again without `cursor`."
        ),
        None,
    )
}

fn foreign_cursor(tool: &str) -> McpError {
    McpError::invalid_params(
        format!(
            "`{tool}`: this cursor belongs to a different query. A cursor continues only the call that \
             returned it, with the same arguments apart from `limit` and `max_tokens`. Call again without \
             `cursor`."
        ),
        None,
    )
}

/// FNV-1a over `bytes`, continuing from `hash`. Stable across builds and platforms, which a cursor
/// handed to another process (the CLI, a restarted server) needs; `ahash` is seeded per process.
fn fnv1a(mut hash: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Identity of the list a call pages through: the tool and every argument that selects it. An
/// absent, `null` or `false` argument, or one holding its default ([`Paged::DEFAULTS`]), is the
/// same query as one that omits it.
fn query_id<T: Paged>(params: &T) -> u32 {
    let mut hash = fnv1a(0x811c_9dc5, T::TOOL.as_bytes());
    let Ok(Value::Object(arguments)) = serde_json::to_value(params) else {
        return hash;
    };
    for (key, value) in &arguments {
        let is_default = || {
            T::DEFAULTS
                .iter()
                .any(|(field, default)| field == key && value.as_str() == Some(default))
        };
        if PAGING_KEYS.contains(&key.as_str()) || value.is_null() || *value == Value::Bool(false) || is_default() {
            continue;
        }
        hash = fnv1a(hash, &[0]);
        hash = fnv1a(hash, key.as_bytes());
        hash = fnv1a(hash, &[0]);
        hash = fnv1a(hash, value.to_string().as_bytes());
    }
    hash
}

fn bind(cursor: &str, id: u32) -> String {
    format!("{id:08x}{SEPARATOR}{cursor}")
}

/// The tool's own cursor inside a bound one, when the binding is this query's.
fn unbind(tool: &str, cursor: &Cursor, id: u32) -> Result<Cursor, McpError> {
    let (prefix, inner) = cursor.0.split_once(SEPARATOR).ok_or_else(|| foreign_cursor(tool))?;
    if u32::from_str_radix(prefix, 16) != Ok(id) || prefix.len() != 8 {
        return Err(foreign_cursor(tool));
    }
    Ok(Cursor(inner.to_string()))
}

/// Run a paginated tool body with its cursor bound to the query.
///
/// Every tool that takes `limit` pages through here, so this is also where `limit: 0` is refused,
/// once and in the same words for all of them.
pub(super) async fn bound<T, F, Fut>(mut params: T, body: F) -> Result<CallToolResult, McpError>
where
    T: Paged,
    F: FnOnce(T) -> Fut,
    Fut: Future<Output = Result<CallToolResult, McpError>>,
{
    super::helpers_filters::reject_zero_limit(T::TOOL, params.limit())?;
    let id = query_id(&params);
    let slot = params.cursor_mut();
    if let Some(cursor) = slot.as_ref() {
        *slot = Some(unbind(T::TOOL, cursor, id)?);
    }
    rebind(T::TOOL, body(params).await?, id)
}

/// Bind the `next_cursor` of a body's response, and turn a `cursor_invalidated` page into the
/// stale-cursor error.
fn rebind(tool: &str, mut result: CallToolResult, id: u32) -> Result<CallToolResult, McpError> {
    let Some(Value::Object(mut body)) = result.structured_content.take() else {
        return Ok(result);
    };
    if body.remove("cursor_invalidated") == Some(Value::Bool(true)) {
        return Err(stale_cursor(tool));
    }
    if let Some(Value::String(cursor)) = body.get_mut("next_cursor") {
        *cursor = bind(cursor, id);
    }
    Ok(CallToolResult::structured(Value::Object(body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types_tools::{CodeOutlineParams, CodeSymbolsParams};
    use serde_json::json;

    fn outline(path: &str, limit: Option<u32>, cursor: Option<Cursor>) -> CodeOutlineParams {
        CodeOutlineParams {
            path: path.into(),
            limit,
            cursor,
            max_tokens: None,
        }
    }

    fn symbols(name: &str, fuzzy: Option<bool>) -> CodeSymbolsParams {
        CodeSymbolsParams {
            name: name.to_string(),
            kind: None,
            fuzzy,
            limit: None,
            cursor: None,
            max_tokens: None,
        }
    }

    async fn page(params: CodeOutlineParams) -> Result<CallToolResult, McpError> {
        bound(params, |inner| async move {
            assert!(
                inner.cursor.as_ref().is_none_or(|cursor| cursor.0 == "inner"),
                "the body sees its own cursor, not the bound one"
            );
            Ok(CallToolResult::structured(json!({ "next_cursor": "inner" })))
        })
        .await
    }

    fn next_cursor(result: &CallToolResult) -> Cursor {
        let body = result.structured_content.as_ref().expect("structured");
        Cursor(body["next_cursor"].as_str().expect("cursor").to_string())
    }

    #[test]
    fn paging_arguments_do_not_change_the_query() {
        let plain = query_id(&outline("a.rs", None, None));
        assert_eq!(plain, query_id(&outline("a.rs", Some(5), Some(Cursor("x".into())))));
        assert_ne!(plain, query_id(&outline("b.rs", None, None)));
    }

    #[test]
    fn an_omitted_flag_and_a_false_one_are_the_same_query() {
        assert_eq!(query_id(&symbols("x", None)), query_id(&symbols("x", Some(false))));
        assert_ne!(query_id(&symbols("x", None)), query_id(&symbols("x", Some(true))));
    }

    /// The defaults are the ones the tool bodies apply: `Relation::parse`, `Matching::parse`, the
    /// `code_search` lane, `Visibility::default` and the blame revision.
    #[test]
    fn an_omitted_argument_and_its_explicit_default_are_the_same_query() {
        use crate::mcp::strict::parse;
        use crate::mcp::types_tools::{CodeRefsParams, CodeSearchParams, GitBlameParams, MemorySearchToolParams};

        fn id<T: Paged>(arguments: serde_json::Value) -> u32 {
            query_id(&parse::<T>(arguments).expect("valid arguments"))
        }

        let refs = id::<CodeRefsParams>(json!({ "name": "x" }));
        assert_eq!(
            refs,
            id::<CodeRefsParams>(json!({ "name": "x", "relation": "callers" }))
        );
        assert_eq!(refs, id::<CodeRefsParams>(json!({ "name": "x", "match": "exact" })));
        assert_eq!(
            refs,
            id::<CodeRefsParams>(json!({ "name": "x", "relation": "callers", "match": "exact" }))
        );
        assert_ne!(refs, id::<CodeRefsParams>(json!({ "name": "x", "match": "substring" })));
        assert_ne!(
            refs,
            id::<CodeRefsParams>(json!({ "name": "x", "relation": "importers" }))
        );
        assert_ne!(refs, id::<CodeRefsParams>(json!({ "name": "callers" })));

        let search = id::<CodeSearchParams>(json!({ "query": "x" }));
        assert_eq!(
            search,
            id::<CodeSearchParams>(json!({ "query": "x", "lane": "hybrid" }))
        );
        assert_ne!(
            search,
            id::<CodeSearchParams>(json!({ "query": "x", "lane": "keyword" }))
        );

        let notes = id::<MemorySearchToolParams>(json!({ "prefix": "a/" }));
        assert_eq!(
            notes,
            id::<MemorySearchToolParams>(json!({ "prefix": "a/", "visibility": "group" }))
        );
        assert_ne!(
            notes,
            id::<MemorySearchToolParams>(json!({ "prefix": "a/", "visibility": "individual" }))
        );

        let blame = id::<GitBlameParams>(json!({ "path": "a.rs" }));
        assert_eq!(blame, id::<GitBlameParams>(json!({ "path": "a.rs", "rev": "HEAD" })));
        assert_ne!(blame, id::<GitBlameParams>(json!({ "path": "a.rs", "rev": "HEAD~1" })));
    }

    #[test]
    fn a_ranked_search_under_another_filter_is_another_query() {
        use crate::mcp::types_tools::DocsSearchParams;
        let id =
            |arguments: Value| query_id(&serde_json::from_value::<DocsSearchParams>(arguments).expect("arguments"));
        let plain = id(json!({ "query": "harbour" }));
        assert_eq!(plain, id(json!({ "query": "harbour", "limit": 3, "max_tokens": 50 })));
        for filtered in [
            json!({ "query": "harbour", "keywords": "pilots" }),
            json!({ "query": "harbour", "entity": "PERSON" }),
            json!({ "query": "harbour", "mime_type": "application/pdf" }),
            json!({ "query": "harbour", "scope": "web:example.org" }),
            json!({ "query": "tides" }),
        ] {
            assert_ne!(plain, id(filtered.clone()), "{filtered}");
        }
    }

    #[test]
    fn the_same_arguments_under_another_tool_are_another_query() {
        #[derive(serde::Deserialize, Serialize, schemars::JsonSchema)]
        struct Twin {
            path: crate::path::RelPath,
            cursor: Option<Cursor>,
        }
        impl ToolArgs for Twin {
            const TOOL: &'static str = "twin";
        }
        impl Paged for Twin {
            fn cursor_mut(&mut self) -> &mut Option<Cursor> {
                &mut self.cursor
            }

            fn limit(&self) -> Option<u32> {
                None
            }
        }
        let twin = Twin {
            path: "a.rs".into(),
            cursor: None,
        };
        assert_ne!(query_id(&twin), query_id(&outline("a.rs", None, None)));
    }

    #[tokio::test]
    async fn a_zero_limit_is_refused_before_the_body_runs() {
        let refused = bound(outline("a.rs", Some(0), None), |_| async {
            panic!("the body must not run for an empty page")
        })
        .await
        .expect_err("limit 0");
        assert_eq!(
            refused.message,
            "`code_outline`: `limit` must be at least 1; omit it for the default page size."
        );
    }

    #[tokio::test]
    async fn a_cursor_continues_its_own_query_and_no_other() {
        let first = page(outline("a.rs", Some(1), None)).await.expect("first page");
        let cursor = next_cursor(&first);
        assert_ne!(cursor.0, "inner", "the cursor handed out is bound");

        page(outline("a.rs", Some(50), Some(cursor.clone())))
            .await
            .expect("same query, another limit");

        let error = page(outline("b.rs", Some(1), Some(cursor))).await.expect_err("foreign");
        assert!(
            error.message.contains("belongs to a different query"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn an_unbound_cursor_is_refused() {
        let error = page(outline("a.rs", None, Some(Cursor("inner".into()))))
            .await
            .expect_err("no binding");
        assert!(
            error.message.contains("belongs to a different query"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn an_invalidated_page_becomes_the_stale_cursor_error() {
        let result = bound(outline("a.rs", None, None), |_| async {
            Ok(CallToolResult::structured(
                json!({ "hits": [], "cursor_invalidated": true }),
            ))
        })
        .await;
        let error = result.expect_err("stale");
        assert!(
            error.message.contains("changed since this cursor was issued"),
            "{}",
            error.message
        );

        let fresh = bound(outline("a.rs", None, None), |_| async {
            Ok(CallToolResult::structured(
                json!({ "hits": [], "cursor_invalidated": false }),
            ))
        })
        .await
        .expect("fresh");
        assert!(
            fresh
                .structured_content
                .as_ref()
                .is_some_and(|body| body.get("cursor_invalidated").is_none())
        );
    }
}
