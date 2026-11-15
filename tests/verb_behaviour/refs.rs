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
    assert_eq!(exact["hits"].as_array().map(Vec::len), Some(2), "{exact}");
    assert_eq!(exact["hits"][0]["callee"], "zap", "{exact}");
    assert!(
        exact["hits"][0]["line"].as_u64().is_some_and(|line| line > 17_000),
        "{exact}"
    );
    assert_ne!(exact["total_is_partial"], true, "{exact}");
    assert!(exact.get("next_cursor").is_none(), "{exact}");
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
        definition["resolution"], "no resolution facts for this file",
        "{definition}"
    );
}
