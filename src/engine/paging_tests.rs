use serde_json::{Value, json};

use super::*;
use crate::engine::types::code::{CodeOutlineParams, CodeSymbolsParams};

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
        path: None,
        language: None,
        fuzzy,
        limit: None,
        cursor: None,
        max_tokens: None,
        changed: None,
    }
}

fn page(params: CodeOutlineParams) -> Result<Value, EngineError> {
    bound_value(params, |inner| {
        assert!(
            inner.cursor.as_ref().is_none_or(|cursor| cursor.0 == "inner"),
            "the body sees its own cursor, not the bound one"
        );
        Ok(json!({ "next_cursor": "inner" }))
    })
}

fn next_cursor(body: &Value) -> Cursor {
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
    use crate::engine::types::git::GitBlameParams;
    use crate::engine::types::memory::MemorySearchToolParams;
    use crate::engine::types::refs::CodeRefsParams;
    use crate::engine::types::search::CodeSearchParams;

    fn id<T: Paged>(arguments: Value) -> u32 {
        query_id(&serde_json::from_value::<T>(arguments).expect("valid arguments"))
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
    use crate::engine::types::search::DocsSearchParams;
    let id = |arguments: Value| query_id(&serde_json::from_value::<DocsSearchParams>(arguments).expect("arguments"));
    let plain = id(json!({ "query": "harbour" }));
    assert_eq!(plain, id(json!({ "query": "harbour", "limit": 3, "max_tokens": 50 })));
    for filtered in [
        json!({ "query": "harbour", "keywords": "pilots" }),
        json!({ "query": "harbour", "entity": "PERSON" }),
        json!({ "query": "harbour", "mime_type": "application/pdf" }),
        json!({ "query": "harbour", "format": "pdf" }),
        json!({ "query": "harbour", "path": "docs/**" }),
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

#[test]
fn a_zero_limit_is_refused_before_the_body_runs() {
    let refused = bound_value(outline("a.rs", Some(0), None), |_| -> Result<Value, EngineError> {
        panic!("the body must not run for an empty page")
    })
    .expect_err("limit 0");
    assert_eq!(
        refused.to_string(),
        "`code_outline`: `limit` must be at least 1; omit it for the default page size."
    );
}

#[test]
fn a_cursor_continues_its_own_query_and_no_other() {
    let first = page(outline("a.rs", Some(1), None)).expect("first page");
    let cursor = next_cursor(&first);
    assert_ne!(cursor.0, "inner", "the cursor handed out is bound");

    page(outline("a.rs", Some(50), Some(cursor.clone()))).expect("same query, another limit");

    let error = page(outline("b.rs", Some(1), Some(cursor))).expect_err("foreign");
    assert!(
        error.to_string().contains("belongs to a different query"),
        "{error}"
    );
}

#[test]
fn an_unbound_cursor_is_refused() {
    let error = page(outline("a.rs", None, Some(Cursor("inner".into())))).expect_err("no binding");
    assert!(
        error.to_string().contains("belongs to a different query"),
        "{error}"
    );
}

#[test]
fn an_invalidated_page_becomes_the_stale_cursor_error() {
    let result = bound_value(outline("a.rs", None, None), |_| {
        Ok(json!({ "hits": [], "cursor_invalidated": true }))
    });
    let error = result.expect_err("stale");
    assert!(
        error.to_string().contains("changed since this cursor was issued"),
        "{error}"
    );

    let fresh = bound_value(outline("a.rs", None, None), |_| {
        Ok(json!({ "hits": [], "cursor_invalidated": false }))
    })
    .expect("fresh");
    assert!(fresh.get("cursor_invalidated").is_none());
}
