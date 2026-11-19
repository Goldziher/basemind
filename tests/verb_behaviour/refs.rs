//! `code_refs` and the other tools that pin one definition: what they match, what they refuse and
//! what they say about resolution.

use std::fmt::Write as _;

use serde_json::json;

use super::{call, call_err, started, started_with};

const TWINS_RS: &str = "\
pub struct Left;
pub struct Right;

impl Left {
    pub fn new() -> Self {
        Left
    }
}

impl Right {
    pub fn new() -> Self {
        Right
    }
}

pub fn build() {
    let _ = Left::new();
}
";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_with_a_path_refuses_a_missing_file_or_symbol() {
    let (_dir, client) = started().await;

    let no_file = call_err(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "path": "src/missing.rs" }),
    )
    .await;
    assert!(no_file.contains("no file `src/missing.rs`"), "{no_file}");

    let no_symbol = call_err(
        &client,
        "code_refs",
        json!({ "name": "nope", "path": "src/outline.rs" }),
    )
    .await;
    assert!(
        no_symbol.contains("no symbol `nope` in `src/outline.rs`"),
        "{no_symbol}"
    );
}

/// Two `new` in one file. Without `line` the pinned forms list the candidates instead of silently
/// taking the first; with `line` they take the one whose definition holds that line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_line_picks_one_of_several_same_named_definitions() {
    let (_dir, client) = started_with(&[("src/twins.rs", TWINS_RS)]).await;

    for (tool, arguments) in [
        ("code_refs", json!({ "name": "new", "path": "src/twins.rs" })),
        ("code_read", json!({ "path": "src/twins.rs", "symbol": "new" })),
        ("git_blame", json!({ "path": "src/twins.rs", "symbol": "new" })),
        ("git_log", json!({ "path": "src/twins.rs", "symbol": "new" })),
    ] {
        let ambiguous = call_err(&client, tool, arguments).await;
        assert!(
            ambiguous.contains("lines 5-7") && ambiguous.contains("lines 11-13"),
            "{tool} lists each candidate with its line range: {ambiguous}"
        );
        assert!(ambiguous.contains("`line`"), "{tool}: {ambiguous}");
    }

    let refs = call(
        &client,
        "code_refs",
        json!({ "name": "new", "path": "src/twins.rs", "line": 12 }),
    )
    .await;
    assert_eq!(refs["definition"]["line"], 11, "{refs}");

    let read = call(
        &client,
        "code_read",
        json!({ "path": "src/twins.rs", "symbol": "new", "line": 11 }),
    )
    .await;
    assert_eq!((read["line"].as_u64(), read["end_line"].as_u64()), (Some(11), Some(13)));
    assert!(read["body"].as_str().unwrap_or_default().contains("Right"), "{read}");

    let blame = call(
        &client,
        "git_blame",
        json!({ "path": "src/twins.rs", "symbol": "new", "line": 6 }),
    )
    .await;
    assert_eq!(
        (blame["line"].as_u64(), blame["end_line"].as_u64()),
        (Some(5), Some(7)),
        "{blame}"
    );

    let log = call(
        &client,
        "git_log",
        json!({ "path": "src/twins.rs", "symbol": "new", "line": 13 }),
    )
    .await;
    assert_eq!(log["history"][0]["change"], "introduced", "{log}");

    let outside = call_err(
        &client,
        "code_refs",
        json!({ "name": "new", "path": "src/twins.rs", "line": 17 }),
    )
    .await;
    assert!(outside.contains("lines 5-7"), "{outside}");
}

const IMPORTS_A_PY: &str = "import os\n\nprint(os.getcwd())\n";
const IMPORTS_B_PY: &str = "import posixpath\n\nprint(posixpath.sep)\n";
const IMPORTS_C_PY: &str = "import os.path\n\nprint(os.path.sep)\n";

/// `os` is not `posixpath`: an importer is a file whose import names the module on a segment
/// boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_importers_match_whole_module_segments() {
    let (_dir, client) = started_with(&[
        ("imports_a.py", IMPORTS_A_PY),
        ("imports_b.py", IMPORTS_B_PY),
        ("imports_c.py", IMPORTS_C_PY),
    ])
    .await;

    let os = call(&client, "code_refs", json!({ "name": "os", "relation": "importers" })).await;
    assert_eq!(os["paths"], json!(["imports_a.py", "imports_c.py"]), "{os}");

    let nested = call(
        &client,
        "code_refs",
        json!({ "name": "os.path", "relation": "importers" }),
    )
    .await;
    assert_eq!(nested["paths"], json!(["imports_c.py"]), "{nested}");

    let posix = call(
        &client,
        "code_refs",
        json!({ "name": "posixpath", "relation": "importers" }),
    )
    .await;
    assert_eq!(posix["paths"], json!(["imports_b.py"]), "{posix}");
}

/// Two exact call sites and more substring-only ones than the old filter-after-scan loop could walk.
/// That loop resolved every one of them, stopped at its round limit and reported the exact total as
/// partial with a cursor leading into empty pages.
fn crowded_rs() -> String {
    let mut source = String::from("pub fn crowd() {\n");
    for _ in 0..17_000 {
        source.push_str("    zapx();\n");
    }
    source.push_str("}\n\npub fn exact() {\n    helper::zap();\n    helper::zap();\n}\n");
    let _ = writeln!(source, "pub fn zapx() {{}}");
    source
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_code_refs_are_not_starved_by_substring_only_callees() {
    let crowded = crowded_rs();
    let (_dir, client) = started_with(&[("src/crowded.rs", crowded.as_str())]).await;

    let exact = call(&client, "code_refs", json!({ "name": "zap", "max_tokens": 100000 })).await;
    assert_eq!(exact["total"], 2, "{exact}");
    let hits = super::rows(&exact["hits"]);
    assert_eq!(hits.len(), 2, "{exact}");
    assert!(
        hits[0].get("callee").is_none(),
        "the callee is the name asked for: {exact}"
    );
    assert!(hits[0]["line"].as_u64().is_some_and(|line| line > 17_000), "{exact}");
    assert!(
        exact.get("truncated").is_none() && exact.get("total_is_partial").is_none(),
        "{exact}"
    );
    assert!(exact.get("next_cursor").is_none(), "{exact}");

    // A scan that stops at its cap before the end of the matches is a hard cut: `total` is then a
    // lower bound, said in the same words every other cut uses.
    let capped = call(
        &client,
        "code_refs",
        json!({ "name": "zap", "match": "substring", "limit": 5 }),
    )
    .await;
    assert_eq!(capped["truncated"], true, "{capped}");
    assert_eq!(capped["truncated_reason"], "scan_cap", "{capped}");
    assert!(capped.get("total_is_partial").is_none(), "{capped}");
    assert!(capped["next_cursor"].is_string(), "{capped}");
}

const PLAIN_PY: &str = "print(1)\n";

/// Resolution support is a property of the language. A file of a supported language that produced
/// no resolution facts is not "unsupported".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supported_language_without_facts_is_not_reported_unsupported() {
    let (_dir, client) = started_with(&[("plain.py", PLAIN_PY)]).await;

    let definition = call(
        &client,
        "code_refs",
        json!({ "name": "print", "relation": "definition", "path": "plain.py", "line": 1 }),
    )
    .await;
    assert!(definition.get("definition").is_none(), "{definition}");
    assert_eq!(
        definition["resolution_note"], "no resolution facts for this file",
        "{definition}"
    );
}

/// "No facts for this file" is only true of an index whose resolve pass finished. While the pass has
/// not run (or died part-way) every file lacks facts, and the answer has to say that instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_whose_resolve_pass_did_not_finish_says_so() {
    let (dir, client) = started_with(&[("plain.py", PLAIN_PY)]).await;
    let definition = json!({ "name": "print", "relation": "definition", "path": "plain.py", "line": 1 });
    let callers = json!({ "name": "helper", "path": "worker.rb" });

    let finished = call(&client, "code_refs", callers.clone()).await;
    assert_eq!(
        finished["resolution_note"], "no resolution facts for this file",
        "{finished}"
    );

    // The scan stamps the sidecar `resolving` before the pass and `complete` after it; this is the
    // state a scan killed in between leaves behind.
    let cache = basemind::store::workspace_cache_dir(dir.path());
    assert!(
        basemind::store::read_status_sidecar(&cache).is_some(),
        "the scan wrote a sidecar"
    );
    basemind::store::write_status_sidecar(&cache, 5, 5, &basemind::store::ScanHealth::resolving());

    for arguments in [definition, callers] {
        let body = call(&client, "code_refs", arguments).await;
        assert_eq!(
            body["resolution_note"],
            "resolution has not finished for this index; a running scan will complete it, otherwise run \
             `index_rescan`",
            "{body}"
        );
    }

    let rust = call(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "path": "src/outline.rs" }),
    )
    .await;
    assert_eq!(
        rust["resolution_note"], "unsupported for rust",
        "a language without an engine: {rust}"
    );
}

const APP_TSX: &str = "import React from 'react';\nexport const app = React;\n";
const DOM_TSX: &str = "import { render } from 'react-dom';\nexport const dom = render;\n";
const RUNTIME_TSX: &str = "import { jsx } from 'react/jsx-runtime';\nexport const runtime = jsx;\n";

/// A hyphen continues a package name: `react-dom` is another package, `react/jsx-runtime` is a
/// path inside `react`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_importers_treat_a_hyphenated_package_as_another_package() {
    let (_dir, client) = started_with(&[
        ("web/app.tsx", APP_TSX),
        ("web/dom.tsx", DOM_TSX),
        ("web/runtime.tsx", RUNTIME_TSX),
    ])
    .await;

    let react = call(
        &client,
        "code_refs",
        json!({ "name": "react", "relation": "importers" }),
    )
    .await;
    assert_eq!(react["paths"], json!(["web/app.tsx", "web/runtime.tsx"]), "{react}");

    let dom = call(
        &client,
        "code_refs",
        json!({ "name": "react-dom", "relation": "importers" }),
    )
    .await;
    assert_eq!(dom["paths"], json!(["web/dom.tsx"]), "{dom}");
}

/// `elsewhere` says resolution looked at a call site and found it binds to another definition. When
/// resolution has no facts about the pinned definition it looked at nothing, and the hits stay
/// `name_only`.
#[cfg(feature = "code-intel-js")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_grades_hits_name_only_when_resolution_knows_nothing_of_the_definition() {
    let (_dir, client) = started_with(&[
        (
            "util.ts",
            "export function target() { return 1; }\ntarget();\nfunction lonely() { return 2; }\n",
        ),
        ("other.ts", "function lonely() { return 3; }\nlonely();\n"),
    ])
    .await;

    let proven = call(&client, "code_refs", json!({ "name": "target", "path": "util.ts" })).await;
    assert_eq!(proven["resolved_total"], 1, "resolution covers util.ts: {proven}");
    assert_eq!(proven["resolution_default"], "same_file", "{proven}");
    assert!(
        super::rows(&proven["hits"])[0].get("resolution").is_none(),
        "a row that is the default leaves it out: {proven}"
    );

    let unknown = call(&client, "code_refs", json!({ "name": "lonely", "path": "util.ts" })).await;
    let hits = super::rows(&unknown["hits"]);
    assert_eq!(hits.len(), 1, "the name scan still reports other.ts: {unknown}");
    assert_eq!(hits[0]["path"], "other.ts", "{unknown}");
    assert!(
        unknown.get("resolution_default").is_none() && hits[0].get("resolution").is_none(),
        "nothing was proven or disproven about this site: it is `name_only`, which goes unwritten: {unknown}"
    );
    assert_eq!(
        unknown["resolution_note"], "no resolution facts for this definition",
        "{unknown}"
    );
    assert!(
        unknown.get("resolved_total").is_none(),
        "a count that was never computed must not be reported as 0: {unknown}"
    );
}

/// Two call sites of a name on one line keep their columns when a page boundary splits them, and a
/// call alone on its line writes none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_pages_split_on_a_shared_line_keep_their_columns() {
    let (_dir, client) = started_with(&[(
        "src/twice.rs",
        "pub fn ping() {}\n\npub fn a() {\n    ping(); ping();\n    ping();\n}\n",
    )])
    .await;
    let mut seen = Vec::new();
    let mut args = json!({ "name": "ping", "limit": 1 });
    for _ in 0..4 {
        let page = call(&client, "code_refs", args.clone()).await;
        for row in super::rows(&page["hits"]) {
            seen.push((row["line"].clone(), row.get("column").cloned()));
        }
        match page["next_cursor"].as_str() {
            Some(cursor) => args["cursor"] = json!(cursor),
            None => break,
        }
    }
    assert_eq!(
        seen,
        [
            (json!(4), Some(json!(5))),
            (json!(4), Some(json!(13))),
            (json!(5), None)
        ],
        "{seen:?}"
    );
}

const ROUTES_RS: &str = "\
pub fn app() -> Router {
    Router::new()
        .route(\"/users/{id}\", get(show_user))
        .route(\"/users/me\", get(me))
}
";

const REQUESTS_RS: &str = "\
pub async fn call(client: &reqwest::Client) {
    reqwest::get(\"https://api.example.com/users/42\").await;
    reqwest::get(\"https://api.example.com/users/me\").await;
    client.get(\"https://api.example.com/v2/users/7\").send().await;
}
";

/// A route's callers are the HTTP calls whose method and path it matches best (ADR-0048): exact
/// before suffix, a literal segment before a parameter. They are graded `path_match` once, as the
/// page's default, and `ranges` merges them into read spans.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_is_called_by_the_http_requests_its_path_matches() {
    let (_dir, client) = started_with(&[("src/routes.rs", ROUTES_RS), ("src/requests.rs", REQUESTS_RS)]).await;

    let callers = call(&client, "code_refs", json!({ "name": "GET /users/:param" })).await;
    assert_eq!(
        callers,
        json!({
            "resolution_default": "path_match",
            "total": 2,
            "hits": { "src/requests.rs": [
                { "line": 2, "callee": "GET /users/42" },
                { "line": 4, "callee": "GET /v2/users/7" }
            ] }
        })
    );

    let pinned = call(
        &client,
        "code_refs",
        json!({ "name": "GET /users/me", "path": "src/routes.rs" }),
    )
    .await;
    assert_eq!(
        pinned["hits"],
        json!({ "src/requests.rs": [{ "line": 3 }] }),
        "the literal route takes `/users/me`: {pinned}"
    );

    let ranged = call(
        &client,
        "code_refs",
        json!({ "name": "GET /users/:param", "ranges": true }),
    )
    .await;
    assert_eq!(
        ranged["hits"],
        json!({ "src/requests.rs": { "ranges": [[1, 5]], "hits": 2 } })
    );

    let definition = call(
        &client,
        "code_refs",
        json!({ "name": "GET /v2/users/7", "relation": "definition", "path": "src/requests.rs", "line": 4 }),
    )
    .await;
    assert_eq!(
        definition,
        json!({
            "path": "src/requests.rs",
            "line": 4,
            "column": 5,
            "definition": { "path": "src/routes.rs", "line": 3, "column": 10, "name": "GET /users/:param" },
            "resolution": "path_match"
        })
    );
}

/// A handler's file and another file defining a function of the same name as the second handler.
const HANDLERS_RS: &str = "pub async fn me() {}\n";
const OTHER_RS: &str = "pub fn me() {}\n";

/// The callers of an HTTP handler are the routes naming it and the HTTP calls those routes answer
/// (ADR-0048 decision 5). A route in the handler's own file names it; one in another file names
/// only some definition of the name, so its rows are `ambiguous` when another file defines it too.
/// An HTTP request is no call of a function its path ends in: `GET /users/me` does not call `me`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handler_is_called_through_its_routes() {
    let routes = format!("{ROUTES_RS}\npub async fn show_user() {{}}\n");
    let (_dir, client) = started_with(&[
        ("src/routes.rs", &routes),
        ("src/requests.rs", REQUESTS_RS),
        ("src/handlers.rs", HANDLERS_RS),
        ("src/other.rs", OTHER_RS),
    ])
    .await;

    let show_user = call(
        &client,
        "code_refs",
        json!({ "name": "show_user", "path": "src/routes.rs" }),
    )
    .await;
    assert_eq!(
        (
            show_user["resolution_default"].clone(),
            show_user["total"].clone(),
            show_user["hits"].clone()
        ),
        (
            json!("path_match"),
            json!(3),
            json!({
                "src/requests.rs": [
                    { "line": 2, "callee": "GET /users/42" },
                    { "line": 4, "callee": "GET /v2/users/7" }
                ],
                "src/routes.rs": [{ "line": 3, "callee": "GET /users/:param" }]
            })
        ),
        "{show_user}"
    );

    let me = call(&client, "code_refs", json!({ "name": "me", "path": "src/handlers.rs" })).await;
    assert_eq!(
        (me["total"].clone(), me["hits"].clone()),
        (
            json!(2),
            json!({
                "src/requests.rs": [{ "line": 3, "callee": "GET /users/me", "ambiguous": true }],
                "src/routes.rs": [{ "line": 4, "callee": "GET /users/me", "ambiguous": true }]
            })
        ),
        "`me` is defined in two files, and the route is in neither: {me}"
    );
    let calls_of_me = call(&client, "code_refs", json!({ "name": "me" })).await;
    assert_eq!(calls_of_me["total"], 0, "{calls_of_me}");
}

/// A route is pinned by its name in any parameter syntax, as it is looked up without `path`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_is_pinned_in_any_parameter_syntax() {
    let (_dir, client) = started_with(&[("src/routes.rs", ROUTES_RS), ("src/requests.rs", REQUESTS_RS)]).await;
    for name in ["GET /users/{id}", "GET /users/:id", "get /users/<id>"] {
        let pinned = call(&client, "code_refs", json!({ "name": name, "path": "src/routes.rs" })).await;
        assert_eq!(
            (pinned["definition"]["name"].clone(), pinned["total"].clone()),
            (json!("GET /users/:param"), json!(2)),
            "{name}: {pinned}"
        );
    }
}

/// An HTTP call is located by a line of the indexed file: once the file is edited, `definition` is
/// refused as for any other reference, and read ranges over it are flagged `stale`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_client_file_is_refused_for_definition_and_its_ranges_are_stale() {
    let (dir, client) = started_with(&[("src/routes.rs", ROUTES_RS), ("src/requests.rs", REQUESTS_RS)]).await;
    let path = dir.path().join("src/requests.rs");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("// one\n// two\n// three\n{source}")).unwrap();

    let message = call_err(
        &client,
        "code_refs",
        json!({ "name": "GET /v2/users/7", "relation": "definition", "path": "src/requests.rs", "line": 4 }),
    )
    .await;
    assert!(
        message.contains("`src/requests.rs` changed since it was indexed"),
        "{message}"
    );

    let ranged = call(
        &client,
        "code_refs",
        json!({ "name": "GET /users/:param", "ranges": true }),
    )
    .await;
    assert_eq!(
        ranged["hits"],
        json!({ "src/requests.rs": { "ranges": [[1, 5]], "hits": 2, "stale": true } })
    );
}
