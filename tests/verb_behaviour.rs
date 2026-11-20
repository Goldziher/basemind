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
#[path = "verb_behaviour/files.rs"]
mod files;
#[path = "verb_behaviour/git.rs"]
mod git;
#[path = "verb_behaviour/graph.rs"]
mod graph;
#[path = "verb_behaviour/grep.rs"]
mod grep;
#[path = "verb_behaviour/history.rs"]
mod history;
#[path = "verb_behaviour/outline.rs"]
mod outline;
#[path = "verb_behaviour/read.rs"]
mod read;
#[path = "verb_behaviour/refs.rs"]
mod refs;
#[path = "verb_behaviour/rescan.rs"]
mod rescan;
#[path = "verb_behaviour/safety.rs"]
mod safety;
#[path = "verb_behaviour/stale.rs"]
mod stale;
#[path = "verb_behaviour/symbols.rs"]
mod symbols;

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
                basemind::retrieval_wiring::backend(),
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
    serve_synced(dir).await
}

async fn serve_synced(dir: TempDir) -> (TempDir, Client) {
    scan(dir.path());
    let client = serve(dir.path()).await;
    history_synced(&client).await;
    (dir, client)
}

/// Wait out the server's boot-time git-history sync. A git walk is served live until the index is
/// up and from the index after, which order commits differently, so a cursor issued across the
/// switch is refused: tests that page through history start once `index_status` stops reporting
/// the sync. The ceiling only stops a hang.
async fn history_synced(client: &Client) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while call(client, "index_status", json!({}))
        .await
        .get("history_syncing")
        .is_some_and(|syncing| syncing == true)
    {
        assert!(std::time::Instant::now() < deadline, "the git-history sync never ended");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
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

/// `tool`'s text block as compact JSON, after checking it is exactly the tool's own encoding of that
/// JSON (ADR-0053), so a byte-for-byte expectation written as JSON pins the text too.
fn json_of(tool: &str, result: &CallToolResult) -> String {
    let text = text_of(result);
    let value = basemind::wire::decode(&text).unwrap_or_else(|error| panic!("{tool}: unreadable ({error}): {text}"));
    assert_eq!(
        basemind::wire::encode(tool, &value),
        text,
        "{tool}: not its own encoding"
    );
    value.to_string()
}

async fn call(client: &Client, tool: &'static str, arguments: Value) -> Value {
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments.as_object().cloned().expect("object")))
        .await
        .unwrap_or_else(|error| panic!("{tool}: {error}"));
    let text = text_of(&result);
    assert_ne!(result.is_error, Some(true), "{tool} {arguments} failed: {text}");
    basemind::wire::decode(&text).unwrap_or_else(|error| panic!("{tool}: unreadable payload ({error}): {text}"))
}

/// A call's tool result, error or not.
async fn call_raw(client: &Client, tool: &'static str, arguments: Value) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments.as_object().cloned().expect("object")))
        .await
        .unwrap_or_else(|error| panic!("{tool}: protocol error instead of a tool result: {error}"))
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

/// The rows of a list `code_grep` and `code_refs` group by file (`{"<path>": [row, ...]}`), each
/// with its `path` put back, in the order listed.
fn rows(grouped: &Value) -> Vec<Value> {
    grouped
        .as_object()
        .unwrap_or_else(|| panic!("hits are grouped by file: {grouped}"))
        .iter()
        .flat_map(|(path, rows)| {
            rows.as_array().expect("a file's rows").iter().map(move |row| {
                let mut row = row.clone();
                row["path"] = json!(path);
                row
            })
        })
        .collect()
}

/// A `code_grep` answer's matches as objects (`path`, `symbol` unless outside every symbol, `line`,
/// `text`, and `column` when the row names columns), one per match: a row naming several columns
/// is one match per column. Context rows (`"<line>- <text>"`) are left out.
fn grep_hits(body: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let files = body["hits"]
        .as_object()
        .unwrap_or_else(|| panic!("hits are grouped by file: {body}"));
    for (path, groups) in files {
        let groups = groups
            .as_object()
            .unwrap_or_else(|| panic!("a file's hits by symbol: {body}"));
        for (symbol, rows) in groups {
            for row in rows.as_array().expect("rows") {
                let row = row.as_str().expect("a row is a string");
                let digits = row.bytes().take_while(u8::is_ascii_digit).count();
                if row.as_bytes().get(digits) != Some(&b':') {
                    continue;
                }
                let line: u64 = row[..digits].parse().expect("a line");
                let (columns, text) = match row[digits + 1..].split_once(": ") {
                    Some((columns, text))
                        if !columns.is_empty() && columns.bytes().all(|b| b.is_ascii_digit() || b == b',') =>
                    {
                        (columns.split(',').map(|c| c.parse::<u64>().unwrap()).collect(), text)
                    }
                    _ => (
                        Vec::new(),
                        row[digits + 1..].strip_prefix(' ').unwrap_or(&row[digits + 1..]),
                    ),
                };
                let mut hit = json!({ "path": path, "line": line, "text": text });
                if !symbol.is_empty() {
                    hit["symbol"] = json!(symbol);
                }
                if columns.is_empty() {
                    out.push(hit);
                    continue;
                }
                for column in columns {
                    let mut hit = hit.clone();
                    hit["column"] = json!(column);
                    out.push(hit);
                }
            }
        }
    }
    out
}

/// A `code_symbols` answer's rows (`"<path>:<line>: <signature>"`), best first.
fn symbol_rows(body: &Value) -> Vec<String> {
    body["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|row| row.as_str().expect("a row is a string").to_string())
        .collect()
}

/// The summaries of a `git_log` list, each commit being a `"<sha> <summary>"` row.
fn commit_summaries(rows: &Value) -> Vec<String> {
    rows.as_array()
        .expect("a list of commit rows")
        .iter()
        .map(|row| {
            row.as_str()
                .expect("a row is a string")
                .split_once(' ')
                .expect("`<sha> <summary>`")
                .1
                .to_string()
        })
        .collect()
}

/// A symbol history row as `(change, summary)`: `"<sha> [introduced] <summary>"` or `[removed]` is
/// that change, a plain `"<sha> <summary>"` (or `[modified]`) is `modified`.
fn history_change(row: &Value) -> (String, String) {
    let rest = row
        .as_str()
        .expect("a row is a string")
        .split_once(' ')
        .expect("`<sha> ...`")
        .1;
    for change in ["introduced", "removed", "modified"] {
        if let Some(summary) = rest.strip_prefix(&format!("[{change}] ")) {
            return (change.to_string(), summary.to_string());
        }
    }
    ("modified".to_string(), rest.to_string())
}

/// A blame answer's hunks as `(lines, summary)`: `lines` is the row's `"<line>[-<end_line>]"`, and
/// the summary is the one `commits` holds for the row's sha.
fn blame_hunks(body: &Value) -> Vec<(String, String)> {
    body["hunks"]
        .as_array()
        .unwrap_or_else(|| panic!("hunks: {body}"))
        .iter()
        .map(|row| {
            let mut parts = row.as_str().expect("a row is a string").split(' ');
            let lines = parts.next().expect("lines");
            let sha = parts.next().expect("sha");
            let summary = body["commits"][sha]["summary"]
                .as_str()
                .unwrap_or_else(|| panic!("no commit {sha} in {body}"));
            (lines.to_string(), summary.to_string())
        })
        .collect()
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
        symbol_rows(&body),
        [
            "src/c_exact.rs:3: pub fn rank_target()",
            "src/b_prefix.rs:1: pub fn rank_target_more()",
            "src/a_substring.rs:1: pub fn pre_rank_target_post()",
        ],
        "rank order is the reverse of path order here, and a row is its 1-based line and signature: {body}"
    );
    assert!(body.get("total").is_none(), "no other page, so no `total`: {body}");

    let first_page = call(&client, "code_symbols", json!({ "name": "rank_target", "limit": 1 })).await;
    assert_eq!(symbol_rows(&first_page), ["src/c_exact.rs:3: pub fn rank_target()"]);
    assert_eq!(
        first_page["total"], 3,
        "a page with a `next_cursor` counts every match: {first_page}"
    );
    let cursor = first_page["next_cursor"].clone();
    assert!(cursor.is_string(), "{first_page}");
    let rest = call(
        &client,
        "code_symbols",
        json!({ "name": "rank_target", "limit": 5, "cursor": cursor }),
    )
    .await;
    assert_eq!(
        symbol_rows(&rest),
        [
            "src/b_prefix.rs:1: pub fn rank_target_more()",
            "src/a_substring.rs:1: pub fn pre_rank_target_post()",
        ],
        "the cursor continues in rank order: {rest}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_symbols_fuzzy_adds_subsequence_matches_below_the_substring_ones() {
    let (_dir, client) = started().await;

    let strict = call(&client, "code_symbols", json!({ "name": "fileoutl2" })).await;
    assert_eq!(strict["results"], json!([]), "not a substring of any name: {strict}");

    let fuzzy = call(&client, "code_symbols", json!({ "name": "fileoutl2", "fuzzy": true })).await;
    let found = symbol_rows(&fuzzy);
    assert_eq!(
        found.first().map(String::as_str),
        Some("src/outline.rs:7: pub fn file_outline_l2() -> u32"),
        "{fuzzy}"
    );

    let ranked = call(&client, "code_symbols", json!({ "name": "render", "fuzzy": true })).await;
    let found = symbol_rows(&ranked);
    // The name is read from the signature after `"<path>:<line>: "`, so a path is never a match.
    let literal = |row: &String| {
        row.split_once(": ")
            .is_some_and(|(_, signature)| signature.contains("render"))
    };
    let first_fuzzy = found.iter().position(|row| !literal(row)).unwrap_or(found.len());
    assert!(
        found[first_fuzzy..].iter().all(|row| !literal(row)),
        "fuzzy matches rank below every literal match: {ranked}"
    );
    assert_eq!(found[0], "src/outline.rs:18: pub fn render(&self) -> u32", "{ranked}");
}

/// A hit is the whole matching line plus the symbol it sits in, so the agent does not have to open
/// the file to learn what it found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_hits_carry_the_full_line_and_the_enclosing_symbol() {
    let (_dir, client) = started().await;

    let body = call(&client, "code_grep", json!({ "pattern": "file_outline() +" })).await;
    assert_eq!(
        body,
        json!({
            "hits": {
                "src/outline.rs": {
                    "render": ["19:         file_outline() + file_outline_l2()"],
                },
            },
        }),
        "a hit is its 1-based line and the full line, under its file and innermost enclosing symbol"
    );

    let outside = call(&client, "code_grep", json!({ "pattern": "outline helpers" })).await;
    assert_eq!(
        outside["hits"],
        json!({ "src/outline.rs": { "": ["1: // outline helpers"] } }),
        "a hit outside every symbol is listed under \"\": {outside}"
    );

    let twice = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline" , "path": "src/outline.rs" }),
    )
    .await;
    assert_eq!(
        twice["hits"]["src/outline.rs"]["render"],
        json!(["19:9,26:         file_outline() + file_outline_l2()"]),
        "two matches on a line are one row naming both columns: {twice}"
    );

    let with_context = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline() +", "context": 1 }),
    )
    .await;
    assert_eq!(
        with_context["hits"],
        json!({
            "src/outline.rs": {
                "render": [
                    "18-     pub fn render(&self) -> u32 {",
                    "19:         file_outline() + file_outline_l2()",
                    "20-     }",
                ],
            },
        }),
        "context lines are rows `<line>- <text>`, as `grep -C` prints them: {with_context}"
    );
}

/// `file_outline` and `file_outline_l2` are different functions. The substring scan counted the
/// call sites of the longer name as callers of the shorter one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_matches_the_callee_name_exactly() {
    let (_dir, client) = started().await;

    let exact = call(&client, "code_refs", json!({ "name": "file_outline" })).await;
    assert_eq!(exact["total"], 2, "one call in each file: {exact}");
    let hits = rows(&exact["hits"]);
    assert!(
        hits.iter().all(|hit| hit.get("callee").is_none()),
        "a callee that is the name asked for is not repeated, and a longer name is a different \
         function: {exact}"
    );
    assert!(
        hits.iter()
            .all(|hit| hit["line"].as_u64().is_some_and(|line| line >= 1) && hit.get("column").is_none()),
        "a call site is its 1-based line: {exact}"
    );
    assert!(exact.get("match").is_none() && exact.get("name").is_none(), "{exact}");

    let longer = call(&client, "code_refs", json!({ "name": "file_outline_l2" })).await;
    assert_eq!(longer["total"], 3, "{longer}");

    let substring = call(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "match": "substring" }),
    )
    .await;
    assert_eq!(substring["total"], 5, "the opt-in keeps the old behaviour: {substring}");
    let longer_callees = rows(&substring["hits"])
        .iter()
        .filter(|hit| hit["callee"] == "file_outline_l2")
        .count();
    assert_eq!(
        longer_callees, 3,
        "a callee other than the name asked for is named: {substring}"
    );
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

/// Where a language has no scope / import resolution, a `resolved_total: 0` would read as "nothing
/// binds to this definition". The response grades every hit `name_only`, says resolution did not run, and tells a language with
/// no resolution engine (rust) from a file of a supported one that bound nothing (the ruby fixture):
/// the scan writes resolution facts only for a file that has some, so their absence alone does not
/// say which.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_says_why_no_hit_was_checked() {
    let (_dir, client) = started().await;

    let rust = call(
        &client,
        "code_refs",
        json!({ "name": "file_outline", "path": "src/outline.rs" }),
    )
    .await;
    assert_eq!(rust["resolution_note"], "unsupported for rust", "{rust}");

    let ruby = call(&client, "code_refs", json!({ "name": "helper", "path": "worker.rb" })).await;
    assert_eq!(ruby["resolution_note"], "no resolution facts for this file", "{ruby}");

    for body in [&rust, &ruby] {
        assert!(
            body.get("resolved_total").is_none(),
            "a count that was never computed must not be reported as 0: {body}"
        );
        assert!(
            body.get("resolution_default").is_none() && body.get("resolution_counts").is_none(),
            "every row is `name_only`, the default a page leaves unwritten: {body}"
        );
        assert!(
            rows(&body["hits"]).iter().all(|hit| hit.get("resolution").is_none()),
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
    assert_eq!(
        blame_hunks(&blame)
            .iter()
            .map(|(lines, _)| lines.as_str())
            .collect::<Vec<_>>(),
        ["7-9"],
        "{blame}"
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
        unresolved["resolution_note"], "unsupported for rust",
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
    assert!(
        added.get("present_at_to").is_none(),
        "a side the file exists at is not said: {added}"
    );
    assert!(
        hunks[0].get("kind").is_none() && diff.get("present_at_from").is_none(),
        "{diff}"
    );

    let since = call(&client, "git_diff", json!({ "path": "src/outline.rs", "from": "HEAD" })).await;
    assert_eq!(
        since["hunks"], diff["hunks"],
        "`from` alone still compares against the working tree"
    );
    assert!(diff.get("lossy_text").is_none(), "every hunk line is UTF-8: {diff}");

    std::fs::write(root.join("latin1.txt"), b"caf\xe9\n").unwrap();
    let latin1 = call(&client, "git_diff", json!({ "path": "latin1.txt" })).await;
    assert_eq!(latin1["lossy_text"], true, "{latin1}");
    assert_eq!(
        latin1["hunks"],
        json!([{ "old_line_start": 1, "old_line_count": 0, "new_line_start": 1, "new_line_count": 1, "text": "caf\u{FFFD}\n" }]),
        "the line that is not UTF-8 is kept, not dropped"
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
        let client = serve(root).await;
        let found = call(&client, "find_files", json!({ "query": "ATTRIBUTIONS" })).await;
        assert_eq!(found["files"][0], "ATTRIBUTIONS.md", "{found}");
        let listed = call(&client, "find_files", json!({ "language": "markdown" })).await;
        assert_eq!(listed["files"][0], "ATTRIBUTIONS.md", "{listed}");
        let _ = client.cancel().await;
    });
}

/// A `docs_search` test checks nothing without an embedding model, so being unable to load one is
/// a failure, not a pass. Where no model can be loaded (an offline machine), setting
/// `BASEMIND_TEST_ALLOW_NO_EMBEDDER` turns it into a reported skip.
#[cfg(feature = "documents")]
fn skip_without_embedder(what: &str) {
    assert!(
        std::env::var_os("BASEMIND_TEST_ALLOW_NO_EMBEDDER").is_some(),
        "{what}. Nothing was checked. Set BASEMIND_TEST_ALLOW_NO_EMBEDDER=1 to skip this test where no \
         embedding model can be loaded."
    );
    eprintln!("SKIP (BASEMIND_TEST_ALLOW_NO_EMBEDDER): {what}");
}

/// With `documents.embed = false` no scan writes vectors, and `docs_search` answers on the lexical
/// lane and names the setting as why the vector lane did not run (ADR-0029: "a search works without
/// the embedder, on the lexical lane, and says so").
#[cfg(feature = "documents")]
#[test]
fn docs_search_answers_lexically_and_names_documents_embed_when_embedding_is_off() {
    let dir = fixture_with(&[]);
    let root = dir.path();
    std::fs::write(
        root.join("guide.md"),
        "# Guide\n\nHow the harbour is run, long enough to index as a document.\n",
    )
    .unwrap();
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
        let client = serve(root).await;
        let answer = call(&client, "docs_search", json!({ "query": "harbour" })).await;
        assert!(
            answer["hits"]
                .as_array()
                .is_some_and(|hits| hits.iter().any(|hit| hit["path"] == "guide.md")),
            "the lexical lane finds the document: {answer}"
        );
        assert_eq!(answer["lanes_ran"], json!(["lexical"]), "{answer}");
        let unavailable = answer["lanes_unavailable"].as_array().cloned().unwrap_or_default();
        assert_eq!(unavailable.len(), 1, "{answer}");
        assert_eq!(unavailable[0]["lane"], "vector", "{answer}");
        assert!(
            unavailable[0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("embed = false")),
            "the vector lane names the setting that turned it off: {answer}"
        );
        let _ = client.cancel().await;
    });
}

/// `[documents.output] format = "toon"` does not apply to `docs_search`: TOON has no structured
/// form to cut a page from, so a page in it would come back whole, unbudgeted and with no
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
            skip_without_embedder(&format!("docs_search could not run (no embedding model?): {text}"));
            let _ = client.cancel().await;
            return;
        }
        let body: Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("the page must be JSON, not TOON ({error}): {text}"));
        let hits = body["hits"].as_array().expect("hits");
        if hits.is_empty() {
            skip_without_embedder(&format!("the scan produced no document vectors: {body}"));
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
