//! `path` means one thing in every search and list tool: a repo-relative prefix, or a glob over the
//! whole path when it holds `*` (`*` within one segment, `**` across them).
//!
//! The fixture puts the same needle and the same `PathConfig` definition under `src/mcp/`, beside it
//! in `src/mcp_types.rs`, under a second `src/mcp/` deeper in `pkg/`, and under a directory whose name
//! holds glob characters, so each case tells a prefix from a substring and a glob from a prefix.

use serde_json::{Value, json};

use super::{Client, call, call_err, grep_hits, started_with};

const NEEDLE_RS: &str = "pub struct PathConfig;\n\npub fn needle() -> &'static str {\n    \"path-needle\"\n}\n";
const NEEDLE_PY: &str = "class PathConfig:\n    NEEDLE = \"path-needle\"\n";
const NEEDLE_TSX: &str = "export class PathConfig {}\nexport const needle = \"path-needle\";\n";

/// Files beside the base fixture, each holding `path-needle` and a `PathConfig` definition.
const FILES: &[(&str, &str)] = &[
    ("src/mcp/tools.rs", NEEDLE_RS),
    ("src/mcp/config.py", NEEDLE_PY),
    ("src/mcp_types.rs", NEEDLE_RS),
    ("pkg/src/mcp/tools.rs", NEEDLE_RS),
    ("app/[id]/page.tsx", NEEDLE_TSX),
];

/// The files of a `code_grep` answer, sorted.
fn grep_files(body: &Value) -> Vec<String> {
    let mut files: Vec<String> = grep_hits(body)
        .iter()
        .map(|hit| hit["path"].as_str().expect("a hit names its file").to_string())
        .collect();
    files.sort_unstable();
    files.dedup();
    files
}

async fn grep_under(client: &Client, path: &str) -> Vec<String> {
    grep_files(&call(client, "code_grep", json!({ "pattern": "path-needle", "path": path })).await)
}

/// The refusal of a `path` that selects no file, for a prefix and for a glob.
fn no_prefix(tool: &str, path: &str) -> String {
    format!(
        "`{tool}`: no indexed path starts with `{path}`; `path` is a repo-relative prefix, or a glob when it \
         holds `*`. List what is indexed with `find_files`."
    )
}

fn no_glob(tool: &str, path: &str) -> String {
    format!(
        "`{tool}`: no indexed path matches the glob `{path}`; `*` stays within one segment and `**` crosses \
         `/`. List what is indexed with `find_files`."
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_path_is_a_prefix_or_a_glob_with_a_star() {
    let (_dir, client) = started_with(FILES).await;

    let cases: &[(&str, &[&str])] = &[
        ("src/mcp/", &["src/mcp/config.py", "src/mcp/tools.rs"]),
        ("src/mc", &["src/mcp/config.py", "src/mcp/tools.rs", "src/mcp_types.rs"]),
        ("app/[id]/", &["app/[id]/page.tsx"]),
        ("src/*.rs", &["src/mcp_types.rs"]),
        ("src/**/*.rs", &["src/mcp/tools.rs", "src/mcp_types.rs"]),
        ("**/mcp/*.rs", &["pkg/src/mcp/tools.rs", "src/mcp/tools.rs"]),
        ("**/*.{py,tsx}", &["app/[id]/page.tsx", "src/mcp/config.py"]),
    ];
    for (path, expected) in cases {
        assert_eq!(grep_under(&client, path).await, *expected, "path {path}");
    }

    // A fragment in the middle of indexed paths is not their prefix.
    assert_eq!(
        call_err(
            &client,
            "code_grep",
            json!({ "pattern": "path-needle", "path": "mcp/" })
        )
        .await,
        no_prefix("code_grep", "mcp/")
    );
    assert_eq!(
        call_err(
            &client,
            "code_grep",
            json!({ "pattern": "path-needle", "path": "src/*.go" })
        )
        .await,
        no_glob("code_grep", "src/*.go")
    );
    let broken = call_err(
        &client,
        "code_grep",
        json!({ "pattern": "path-needle", "path": "src/[*.rs" }),
    )
    .await;
    assert!(
        broken.starts_with("`code_grep`: `path` holds `*`, so it is read as a glob, and `src/[*.rs` is not one ("),
        "{broken}"
    );
}

/// `find_files` paths as listed: a list of paths, or with `min_lines` the keys of its map.
async fn files(client: &Client, arguments: Value) -> Vec<String> {
    let body = call(client, "find_files", arguments).await;
    match &body["files"] {
        Value::Array(rows) => rows
            .iter()
            .map(|row| row.as_str().expect("a path").to_string())
            .collect(),
        Value::Object(sized) => sized.keys().cloned().collect(),
        other => panic!("files: {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_files_path_is_a_prefix_or_a_glob_with_a_star() {
    let (_dir, client) = started_with(FILES).await;

    let cases: &[(Value, &[&str])] = &[
        (
            json!({ "path": "src/mcp/" }),
            &["src/mcp/config.py", "src/mcp/tools.rs"],
        ),
        (
            json!({ "path": "src/mc" }),
            &["src/mcp/config.py", "src/mcp/tools.rs", "src/mcp_types.rs"],
        ),
        (json!({ "path": "app/[id]/" }), &["app/[id]/page.tsx"]),
        (
            json!({ "path": "src/*.rs" }),
            &["src/callers.rs", "src/mcp_types.rs", "src/outline.rs"],
        ),
        (
            json!({ "path": "**/mcp/*.rs" }),
            &["pkg/src/mcp/tools.rs", "src/mcp/tools.rs"],
        ),
        // The filter applies before the fuzzy ranking: the top-level `tools.py` is not under `src/`.
        (json!({ "query": "tools", "path": "src/**" }), &["src/mcp/tools.rs"]),
        (json!({ "min_lines": 1, "path": "src/mcp/*.rs" }), &["src/mcp/tools.rs"]),
    ];
    for (arguments, expected) in cases {
        assert_eq!(files(&client, arguments.clone()).await, *expected, "{arguments}");
    }

    for (arguments, expected) in [
        (json!({ "path": "mcp/" }), no_prefix("find_files", "mcp/")),
        (
            json!({ "query": "tools", "path": "mcp/" }),
            no_prefix("find_files", "mcp/"),
        ),
        (
            json!({ "min_lines": 1, "path": "mcp/" }),
            no_prefix("find_files", "mcp/"),
        ),
        (json!({ "path": "*.go" }), no_glob("find_files", "*.go")),
    ] {
        assert_eq!(
            call_err(&client, "find_files", arguments.clone()).await,
            expected,
            "{arguments}"
        );
    }
    let renamed = call_err(&client, "find_files", json!({ "path_prefix": "src/" })).await;
    assert!(
        renamed.contains("`path`"),
        "the old name points at the new one: {renamed}"
    );
}

/// A markdown document about the harbour; `topic` keeps each one's content, and so its hit, apart.
#[cfg(feature = "documents")]
fn harbour_md(topic: &str) -> String {
    format!("# {topic}\n\nNotes about the harbour and how its {topic} is run, long enough to index.\n")
}

/// `docs_search` over three documents that all mention the harbour, through the lexical lane
/// (embeddings off, so no model is needed).
#[cfg(feature = "documents")]
#[test]
fn docs_search_path_is_a_prefix_or_a_glob_with_a_star() {
    let (guide, ops, notes) = (harbour_md("guide"), harbour_md("quay"), harbour_md("ferry"));
    let dir = super::fixture_with(&[
        ("docs/guide.md", guide.as_str()),
        ("docs/ops/harbour.md", ops.as_str()),
        ("notes/docs/guide.md", notes.as_str()),
    ]);
    let root = dir.path();
    std::fs::write(
        root.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n\n[documents]\nenabled = true\nembed = false\n",
    )
    .unwrap();
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.enabled = true;
    cfg.documents.embed = false;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    {
        let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
        basemind::scanner::scan(
            root,
            &mut store,
            &cfg,
            basemind::scanner::ScanSource::WorkingTree,
            basemind::scanner::EmbedMode::Inline,
            basemind::retrieval_wiring::backend(),
        )
        .expect("scan");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let client = super::serve(root).await;
        let found = |path: &'static str| {
            let client = &client;
            async move {
                let body = call(client, "docs_search", json!({ "query": "harbour", "path": path })).await;
                let mut paths: Vec<String> = body["hits"]
                    .as_array()
                    .expect("hits")
                    .iter()
                    .map(|hit| hit["path"].as_str().expect("a hit names its document").to_string())
                    .collect();
                paths.sort_unstable();
                paths
            }
        };
        assert_eq!(found("docs/").await, ["docs/guide.md", "docs/ops/harbour.md"]);
        assert_eq!(found("docs/g").await, ["docs/guide.md"]);
        assert_eq!(found("docs/*.md").await, ["docs/guide.md"]);
        assert_eq!(found("**/guide.md").await, ["docs/guide.md", "notes/docs/guide.md"]);
        assert!(
            found("ops/").await.is_empty(),
            "a fragment inside a path is not its prefix"
        );
        let broken = call_err(&client, "docs_search", json!({ "query": "harbour", "path": "docs/[*" })).await;
        assert!(
            broken.starts_with("`docs_search`: `path` holds `*`, so it is read as a glob, and `docs/[*` is not one ("),
            "{broken}"
        );
        let _ = client.cancel().await;
    });
}

/// The files of a `code_symbols` answer, from rows `"<path>:<line>: <signature>"`, sorted.
async fn symbol_files(client: &Client, arguments: Value) -> Vec<String> {
    let body = call(client, "code_symbols", arguments).await;
    let mut files: Vec<String> = super::symbol_rows(&body)
        .iter()
        .map(|row| row.split_once(':').expect("a row starts with its path").0.to_string())
        .collect();
    files.sort_unstable();
    files.dedup();
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_symbols_path_and_language_scope_the_definitions() {
    let (_dir, client) = started_with(FILES).await;

    let cases: &[(Value, &[&str])] = &[
        (
            json!({ "name": "PathConfig" }),
            &[
                "app/[id]/page.tsx",
                "pkg/src/mcp/tools.rs",
                "src/mcp/config.py",
                "src/mcp/tools.rs",
                "src/mcp_types.rs",
            ],
        ),
        (
            json!({ "name": "PathConfig", "path": "src/mcp/" }),
            &["src/mcp/config.py", "src/mcp/tools.rs"],
        ),
        (
            json!({ "name": "PathConfig", "path": "src/mc" }),
            &["src/mcp/config.py", "src/mcp/tools.rs", "src/mcp_types.rs"],
        ),
        (
            json!({ "name": "PathConfig", "path": "**/mcp/*.rs" }),
            &["pkg/src/mcp/tools.rs", "src/mcp/tools.rs"],
        ),
        (
            json!({ "name": "PathConfig", "path": "src/mcp/", "language": "rust" }),
            &["src/mcp/tools.rs"],
        ),
        (
            json!({ "name": "PathConfig", "language": "python" }),
            &["src/mcp/config.py"],
        ),
        (
            json!({ "name": "PathConfig", "path": "app/[id]/" }),
            &["app/[id]/page.tsx"],
        ),
        // Files pass the filters, none defines the name: an empty answer, not a refusal.
        (json!({ "name": "PathConfig", "path": "src/outline.rs" }), &[]),
    ];
    for (arguments, expected) in cases {
        assert_eq!(symbol_files(&client, arguments.clone()).await, *expected, "{arguments}");
    }

    for (arguments, expected) in [
        (
            json!({ "name": "PathConfig", "path": "mcp/" }),
            no_prefix("code_symbols", "mcp/"),
        ),
        (
            json!({ "name": "PathConfig", "path": "*.go" }),
            no_glob("code_symbols", "*.go"),
        ),
    ] {
        assert_eq!(
            call_err(&client, "code_symbols", arguments.clone()).await,
            expected,
            "{arguments}"
        );
    }
    let language = call_err(
        &client,
        "code_symbols",
        json!({ "name": "PathConfig", "language": "cobol" }),
    )
    .await;
    assert!(
        language.starts_with("`code_symbols`: no indexed file has language `cobol`."),
        "{language}"
    );
}

/// The keyword lane needs no embedding model, so this runs wherever `code_search` is built.
#[cfg(feature = "code-search")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_search_path_and_language_scope_the_hits() {
    let (_dir, client) = started_with(FILES).await;
    let hits = |arguments: Value| {
        let client = &client;
        async move {
            let mut arguments = arguments;
            arguments["query"] = json!("needle");
            arguments["lane"] = json!("keyword");
            let body = call(client, "code_search", arguments).await;
            let mut files: Vec<String> = Vec::new();
            for hit in body["hits"].as_array().expect("hits") {
                files.push(hit["path"].as_str().expect("a hit names its file").to_string());
                for copy in hit["also_at"].as_array().into_iter().flatten() {
                    files.push(copy.as_str().expect("a copy is a path").to_string());
                }
            }
            files.sort_unstable();
            files.dedup();
            files
        }
    };

    assert_eq!(
        hits(json!({ "path": "src/mcp/" })).await,
        ["src/mcp/config.py", "src/mcp/tools.rs"]
    );
    assert_eq!(
        hits(json!({ "path": "**/mcp/*.rs" })).await,
        ["pkg/src/mcp/tools.rs", "src/mcp/tools.rs"]
    );
    assert_eq!(
        hits(json!({ "path": "src/mc", "language": "rust" })).await,
        ["src/mcp/tools.rs", "src/mcp_types.rs"]
    );
    assert_eq!(hits(json!({ "path": "app/[id]/" })).await, ["app/[id]/page.tsx"]);
    assert_eq!(
        call_err(
            &client,
            "code_search",
            json!({ "query": "needle", "lane": "keyword", "path": "mcp/" })
        )
        .await,
        no_prefix("code_search", "mcp/")
    );
}
