//! Behaviour of the verb tools that the mode-dispatched surface got wrong (ADR-0018): ranking,
//! positions, exact matching, the working-tree diff and what `index_status` reports.
//!
//! Each test drives the real server over an in-memory transport against a scanned git fixture.

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use serde_json::{Value, json};
use tempfile::TempDir;

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

#[path = "verb_behaviour/args.rs"]
mod args;
#[path = "verb_behaviour/bounds.rs"]
mod bounds;
#[path = "verb_behaviour/cursors.rs"]
mod cursors;
#[path = "verb_behaviour/grep.rs"]
mod grep;
#[path = "verb_behaviour/history.rs"]
mod history;
#[path = "verb_behaviour/refs.rs"]
mod refs;
#[path = "verb_behaviour/safety.rs"]
mod safety;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

const OUTLINE_RS: &str = "\
// outline helpers

pub fn file_outline() -> u32 {
    1
}

pub fn file_outline_l2() -> u32 {
    2
}

pub fn outline_file_outline_cached() -> u32 {
    3
}

pub struct Renderer;

impl Renderer {
    pub fn render(&self) -> u32 {
        file_outline() + file_outline_l2()
    }
}
";

const CALLERS_RS: &str = "\
use crate::outline::file_outline;

pub fn first() -> u32 {
    file_outline()
}

pub fn second() -> u32 {
    crate::outline::file_outline_l2() + crate::outline::file_outline_l2()
}
";

const WORKER_RB: &str = "\
def helper
  1
end

def run
  helper
end
";

const TOOLS_PY: &str = "\
def run(value):
    total = value + 1
    return total
";

/// The base fixture plus `extra` files (repo-relative path, contents), all in the first commit.
fn fixture_with(extra: &[(&str, &str)]) -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/outline.rs"), OUTLINE_RS).unwrap();
    std::fs::write(root.join("src/callers.rs"), CALLERS_RS).unwrap();
    std::fs::write(root.join("worker.rb"), WORKER_RB).unwrap();
    std::fs::write(root.join("tools.py"), TOOLS_PY).unwrap();
    for (path, contents) in extra {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).unwrap();
        std::fs::write(file, contents).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

fn scan(root: &Path) {
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
            )
            .expect("scan");
        });
    });
}

async fn serve(root: &Path) -> Client {
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    ().serve(transport).await.expect("rmcp handshake")
}

async fn started() -> (TempDir, Client) {
    started_with(&[]).await
}

async fn started_with(extra: &[(&str, &str)]) -> (TempDir, Client) {
    let dir = fixture_with(extra);
    scan(dir.path());
    let client = serve(dir.path()).await;
    (dir, client)
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

async fn call(client: &Client, tool: &'static str, arguments: Value) -> Value {
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments.as_object().cloned().expect("object")))
        .await
        .unwrap_or_else(|error| panic!("{tool}: {error}"));
    let text = text_of(&result);
    assert_ne!(result.is_error, Some(true), "{tool} {arguments} failed: {text}");
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{tool}: non-JSON payload ({error}): {text}"))
}

/// The message of a call that must come back as an `isError` tool result (never a protocol error).
async fn call_err(client: &Client, tool: &'static str, arguments: Value) -> String {
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments.as_object().cloned().expect("object")))
        .await
        .unwrap_or_else(|error| panic!("{tool}: protocol error instead of a tool result: {error}"));
    let text = text_of(&result);
    assert_eq!(
        result.is_error,
        Some(true),
        "{tool} {arguments} must be refused: {text}"
    );
    text
}

fn names(rows: &Value) -> Vec<String> {
    rows.as_array()
        .expect("array")
        .iter()
        .map(|row| row["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The exact name comes first, then names it prefixes, then names that merely contain it. The old
/// substring sweep returned them in file order, so the definition asked for was not the first row.
///
/// The three names are declared in the REVERSE of rank order, one per file, with the files sorting
/// substring, prefix, exact: any order derived from paths or declaration order fails this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_symbols_ranks_exact_then_prefix_then_substring() {
    let (_dir, client) = started_with(&[
        ("src/a_substring.rs", "pub fn pre_rank_target_post() {}\n"),
        ("src/b_prefix.rs", "pub fn rank_target_more() {}\n"),
        (
            "src/c_exact.rs",
            "// the definition asked for\n\npub fn rank_target() {}\n",
        ),
    ])
    .await;

    let body = call(&client, "code_symbols", json!({ "name": "rank_target" })).await;
    assert_eq!(
        names(&body["results"]),
        ["rank_target", "rank_target_more", "pre_rank_target_post"],
        "{body}"
    );
    let matches: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["match"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(matches, ["exact", "prefix", "substring"], "{body}");
    let paths: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        paths,
        ["src/c_exact.rs", "src/b_prefix.rs", "src/a_substring.rs"],
        "rank order is the reverse of path order here: {body}"
    );
    assert_eq!(body["total"], 3, "{body}");

    let exact = &body["results"][0];
    assert_eq!(exact["line"], 3, "rows carry a 1-based `line`: {exact}");
    assert!(
        exact.get("start_row").is_none(),
        "the 0-based row must be gone: {exact}"
    );

    let first_page = call(&client, "code_symbols", json!({ "name": "rank_target", "limit": 1 })).await;
    assert_eq!(names(&first_page["results"]), ["rank_target"]);
    let cursor = first_page["next_cursor"].clone();
    assert!(cursor.is_string(), "{first_page}");
    let rest = call(
        &client,
        "code_symbols",
        json!({ "name": "rank_target", "limit": 5, "cursor": cursor }),
    )
    .await;
    assert_eq!(
        names(&rest["results"]),
        ["rank_target_more", "pre_rank_target_post"],
        "the cursor continues in rank order: {rest}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_symbols_fuzzy_adds_subsequence_matches_below_the_substring_ones() {
    let (_dir, client) = started().await;

    let strict = call(&client, "code_symbols", json!({ "name": "fileoutl2" })).await;
    assert_eq!(strict["total"], 0, "not a substring of any name: {strict}");

    let fuzzy = call(&client, "code_symbols", json!({ "name": "fileoutl2", "fuzzy": true })).await;
    let found = names(&fuzzy["results"]);
    assert_eq!(found.first().map(String::as_str), Some("file_outline_l2"), "{fuzzy}");
    assert_eq!(fuzzy["results"][0]["match"], "fuzzy", "{fuzzy}");

    let ranked = call(&client, "code_symbols", json!({ "name": "render", "fuzzy": true })).await;
    let kinds: Vec<&str> = ranked["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["match"].as_str().unwrap_or_default())
        .collect();
    let first_fuzzy = kinds.iter().position(|kind| *kind == "fuzzy").unwrap_or(kinds.len());
    assert!(
        kinds[first_fuzzy..].iter().all(|kind| *kind == "fuzzy"),
        "fuzzy matches rank below every literal match: {ranked}"
    );
    assert_eq!(names(&ranked["results"])[0], "render", "{ranked}");
}

/// A hit is the whole matching line plus the symbol it sits in, so the agent does not have to open
/// the file to learn what it found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_hits_carry_the_full_line_and_the_enclosing_symbol() {
    let (_dir, client) = started().await;

    let body = call(&client, "code_grep", json!({ "pattern": "file_outline() +" })).await;
    assert_eq!(body["total_matches"], 1, "{body}");
    let hit = &body["hits"][0];
    assert_eq!(hit["path"], "src/outline.rs");
    assert_eq!(hit["line"], 19, "1-based line: {hit}");
    assert_eq!(hit["column"], 9, "1-based column of the match: {hit}");
    assert_eq!(
        hit["text"], "        file_outline() + file_outline_l2()",
        "the full line, not the matched substring: {hit}"
    );
    assert_eq!(hit["symbol"]["name"], "render", "the innermost enclosing symbol: {hit}");
    assert_eq!(hit["symbol"]["kind"], "function", "{hit}");
    assert!(
        hit.get("matched_text").is_none() && hit.get("line_num").is_none(),
        "{hit}"
    );

    let outside = call(&client, "code_grep", json!({ "pattern": "outline helpers" })).await;
    assert!(
        outside["hits"][0].get("symbol").is_none(),
        "a hit outside every symbol names none: {outside}"
    );

    let with_context = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline() +", "context": true }),
    )
    .await;
    assert_eq!(
        with_context["hits"][0]["context_before"], "    pub fn render(&self) -> u32 {",
        "{with_context}"
    );
}

/// `file_outline` and `file_outline_l2` are different functions. The substring scan counted the
/// call sites of the longer name as callers of the shorter one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_matches_the_callee_name_exactly() {
    let (_dir, client) = started().await;

    let exact = call(&client, "code_refs", json!({ "name": "file_outline" })).await;
    let callees: Vec<&str> = exact["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["callee"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(exact["total"], 2, "one call in each file: {exact}");
    assert!(
        callees.iter().all(|callee| !callee.contains("file_outline_l2")),
        "a longer name is a different function: {exact}"
    );
    assert_eq!(exact["match"], "exact", "{exact}");
    let first = &exact["hits"][0];
    assert!(
        first["line"].as_u64().is_some_and(|line| line >= 1) && first["column"].as_u64().is_some_and(|col| col >= 1),
        "positions are 1-based: {first}"
    );

    let longer = call(&client, "code_refs", json!({ "name": "file_outline_l2" })).await;
    assert_eq!(longer["total"], 3, "{longer}");

    let substring = call(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "match": "substring" }),
    )
    .await;
    assert_eq!(substring["total"], 5, "the opt-in keeps the old behaviour: {substring}");
    assert_eq!(substring["match"], "substring");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_with_a_path_pins_the_definition_and_is_exact_too() {
    let (_dir, client) = started().await;

    let pinned = call(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "path": "src/outline.rs" }),
    )
    .await;
    assert_eq!(pinned["definition"]["name"], "file_outline", "{pinned}");
    assert_eq!(pinned["definition"]["line"], 3, "1-based definition line: {pinned}");
    assert!(pinned["definition"].get("start_row").is_none(), "{pinned}");
    assert_eq!(pinned["total"], 2, "exact, like the unpinned scan: {pinned}");
}

/// Where a language has no scope / import resolution, `resolved_total: 0` used to read as "nothing
/// binds to this definition". The response says resolution did not run, and tells a language with
/// no resolution engine (rust) from a file of a supported one that bound nothing (the ruby fixture):
/// the scan writes resolution facts only for a file that has some, so their absence alone does not
/// say which.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_says_why_no_hit_is_marked_resolved() {
    let (_dir, client) = started().await;

    let rust = call(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "path": "src/outline.rs" }),
    )
    .await;
    assert_eq!(rust["resolution"], "unsupported for rust", "{rust}");

    let ruby = call(&client, "code_refs", json!({ "name": "helper", "path": "worker.rb" })).await;
    assert_eq!(ruby["resolution"], "no resolution facts for this file", "{ruby}");

    for body in [&rust, &ruby] {
        assert!(
            body.get("resolved_total").is_none(),
            "a count that was never computed must not be reported as 0: {body}"
        );
        assert!(
            body["hits"]
                .as_array()
                .is_some_and(|hits| hits.iter().all(|hit| hit.get("resolved").is_none())),
            "{body}"
        );
    }
}

/// The line `code_outline` reports for a symbol is the line `code_read` and `git_blame` use for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outline_read_and_blame_agree_on_a_symbols_lines() {
    let (_dir, client) = started().await;

    let outline = call(&client, "code_outline", json!({ "path": "src/outline.rs" })).await;
    let symbol = outline["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "file_outline_l2")
        .expect("file_outline_l2 in the outline")
        .clone();
    assert_eq!(
        (symbol["line"].as_u64(), symbol["end_line"].as_u64()),
        (Some(7), Some(9))
    );

    let read = call(
        &client,
        "code_read",
        json!({ "path": "src/outline.rs", "symbol": "file_outline_l2" }),
    )
    .await;
    assert_eq!(
        (read["line"].clone(), read["end_line"].clone()),
        (symbol["line"].clone(), symbol["end_line"].clone())
    );

    let blame = call(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "symbol": "file_outline_l2" }),
    )
    .await;
    assert_eq!(
        (blame["line"].clone(), blame["end_line"].clone()),
        (symbol["line"].clone(), symbol["end_line"].clone()),
        "{blame}"
    );
    let hunk = &blame["hunks"][0];
    assert_eq!(
        (hunk["line"].as_u64(), hunk["end_line"].as_u64()),
        (Some(7), Some(9)),
        "{hunk}"
    );

    let span = call(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "line": 7, "end_line": 9 }),
    )
    .await;
    assert_eq!(
        span["hunks"], blame["hunks"],
        "a symbol blame is the blame of its outline span"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_definition_takes_and_returns_one_based_positions() {
    let (_dir, client) = started().await;

    let by_name = call(
        &client,
        "code_refs",
        json!({ "name": "total", "relation": "definition", "path": "tools.py", "line": 3 }),
    )
    .await;
    assert_eq!(by_name["line"], 3);
    assert_eq!(
        by_name["column"], 12,
        "the column of `total` on that line, 1-based: {by_name}"
    );
    assert_eq!(by_name["definition"]["path"], "tools.py", "{by_name}");
    assert_eq!(by_name["definition"]["line"], 2, "{by_name}");
    assert_eq!(by_name["definition"]["column"], 5, "`    total = …`: {by_name}");
    assert_eq!(by_name["definition"]["name"], "total", "{by_name}");

    let by_column = call(
        &client,
        "code_refs",
        json!({ "name": "total", "relation": "definition", "path": "tools.py", "line": 3, "column": 12 }),
    )
    .await;
    assert_eq!(by_column["definition"], by_name["definition"]);

    let unresolved = call(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "relation": "definition", "path": "src/outline.rs", "line": 19 }),
    )
    .await;
    assert_eq!(unresolved["column"], 9, "{unresolved}");
    assert!(unresolved.get("definition").is_none(), "{unresolved}");
    assert_eq!(
        unresolved["resolution"], "unsupported for rust",
        "no binding is reported as resolution not having run, not as a silent miss: {unresolved}"
    );
}

/// There was no working-tree diff at all: `diff` required two revisions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_without_revisions_describes_the_working_tree() {
    let (dir, client) = started().await;
    let root = dir.path();

    let clean = call(&client, "git_diff", json!({})).await;
    assert_eq!(clean["is_clean"], true, "{clean}");

    std::fs::write(root.join("src/outline.rs"), OUTLINE_RS.replace("    1\n", "    100\n")).unwrap();
    std::fs::write(root.join("new.rs"), "pub fn fresh() {}\n").unwrap();

    let status = call(&client, "git_diff", json!({})).await;
    assert_eq!(status["is_clean"], false, "{status}");
    assert_eq!(status["modified"], json!(["src/outline.rs"]), "{status}");
    assert_eq!(status["untracked"], json!(["new.rs"]), "{status}");

    let diff = call(&client, "git_diff", json!({ "path": "src/outline.rs" })).await;
    assert_eq!(diff["to"], "working tree", "{diff}");
    assert!(
        diff["from"].as_str().is_some_and(|sha| sha.len() == 40),
        "HEAD, resolved: {diff}"
    );
    let hunks = diff["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1, "{diff}");
    let text = hunks[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("-    1") && text.contains("+    100"), "{diff}");

    let unchanged = call(&client, "git_diff", json!({ "path": "src/callers.rs" })).await;
    assert_eq!(unchanged["hunks"], json!([]), "{unchanged}");

    let added = call(&client, "git_diff", json!({ "path": "new.rs" })).await;
    assert_eq!(added["present_at_from"], false, "{added}");
    assert_eq!(added["present_at_to"], true, "{added}");

    let since = call(&client, "git_diff", json!({ "path": "src/outline.rs", "from": "HEAD" })).await;
    assert_eq!(
        since["hunks"], diff["hunks"],
        "`from` alone still compares against the working tree"
    );
}

/// `cache_dir` reported the tree-sitter grammar directory, which is not where basemind keeps
/// anything of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_status_reports_basemind_s_cache_directory() {
    let (dir, client) = started().await;

    let status = call(&client, "index_status", json!({})).await;
    let cache_dir = status["cache_dir"].as_str().expect("cache_dir");
    let expected = basemind::store::workspace_cache_dir(dir.path());
    assert_eq!(Path::new(cache_dir), expected.as_path(), "{status}");
    assert!(
        Path::new(cache_dir).join("views").is_dir(),
        "the directory holds this workspace's index views: {cache_dir}"
    );
    assert!(!cache_dir.contains("tree-sitter"), "{cache_dir}");
    assert!(
        status["grammar_dir"].as_str().is_some_and(|dir| !dir.is_empty()),
        "the grammar directory keeps its own field: {status}"
    );
    assert!(
        status["branch"].is_string() && status["head_sha"].is_string(),
        "{status}"
    );
}

/// Markdown and other documents are files too. Needs the document tier, which is what indexes them.
#[cfg(feature = "documents")]
#[test]
fn find_files_returns_a_markdown_file() {
    let dir = fixture_with(&[]);
    let root = dir.path();
    std::fs::write(
        root.join("ATTRIBUTIONS.md"),
        "# Attributions\n\nThird-party notices for the fixture, long enough to index.\n",
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
        )
        .expect("scan");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let client = serve(root).await;
        let found = call(&client, "find_files", json!({ "query": "ATTRIBUTIONS" })).await;
        assert_eq!(found["files"][0]["path"], "ATTRIBUTIONS.md", "{found}");
        assert_eq!(found["files"][0]["language"], "markdown", "{found}");
        let listed = call(&client, "find_files", json!({ "language": "markdown" })).await;
        assert_eq!(listed["files"][0]["path"], "ATTRIBUTIONS.md", "{listed}");
        let _ = client.cancel().await;
    });
}

/// `[documents.output] format = "toon"` used to switch the first `docs_search` page to TOON, which
/// has no structured form to cut a page from: the page came back whole, unbudgeted and with no
/// `next_cursor`. The verb tools answer in JSON whatever that setting says.
#[cfg(feature = "documents")]
#[test]
fn docs_search_pages_as_json_when_the_config_asks_for_toon() {
    let dir = fixture_with(&[]);
    let root = dir.path();
    for (name, topic) in [
        ("alpha", "harbour"),
        ("beta", "harbour pilots"),
        ("gamma", "harbour tides"),
    ] {
        std::fs::write(
            root.join(format!("{name}.md")),
            format!("# {topic}\n\nNotes about the {topic} and how the {topic} is run, long enough to index.\n"),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n\n[documents]\nenabled = true\n\n[documents.output]\nformat = \"toon\"\n",
    )
    .unwrap();
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.enabled = true;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    {
        let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
        basemind::scanner::scan(
            root,
            &mut store,
            &cfg,
            basemind::scanner::ScanSource::WorkingTree,
            basemind::scanner::EmbedMode::Inline,
        )
        .expect("scan");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let client = serve(root).await;
        let result = client
            .call_tool(
                CallToolRequestParams::new("docs_search")
                    .with_arguments(json!({ "query": "harbour", "limit": 1 }).as_object().cloned().unwrap()),
            )
            .await
            .expect("docs_search");
        let text = text_of(&result);
        if result.is_error == Some(true) {
            eprintln!("SKIP: docs_search could not run here (no embedding model?): {text}");
            let _ = client.cancel().await;
            return;
        }
        let body: Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("the page must be JSON, not TOON ({error}): {text}"));
        let hits = body["hits"].as_array().expect("hits");
        if hits.is_empty() {
            eprintln!("SKIP: no document vectors in this environment: {body}");
            let _ = client.cancel().await;
            return;
        }
        assert_eq!(hits.len(), 1, "`limit` is honoured on the first page: {body}");
        assert!(
            body["next_cursor"].is_string(),
            "three documents match, so a page follows: {body}"
        );
        let _ = client.cancel().await;
    });
}
