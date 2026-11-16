//! What `code_grep` matches, which files it reads and what it says about the ones it could not.

use serde_json::json;

use super::{call, rows, started, started_with};

/// `^` and `$` anchor at every line, as in any grep. Anchored to the file instead, `^pub fn ` could
/// match once per file at most and only on its first line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_regex_anchors_match_at_every_line() {
    let (_dir, client) = started_with(&[("src/crlf.rs", "pub fn one() {}\r\npub fn two() {}\r\n")]).await;

    let starts = call(
        &client,
        "code_grep",
        json!({ "pattern": "^pub fn ", "regex": true, "path": "src/" }),
    )
    .await;
    assert_eq!(
        starts["total"], 7,
        "3 in outline.rs, 2 in callers.rs, 2 in crlf.rs; the indented `pub fn render` is not one: {starts}"
    );
    assert_eq!(starts["hits"].as_object().map(|files| files.len()), Some(3), "{starts}");
    let lines: Vec<(String, u64)> = starts["hits"]["src/outline.rs"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| (hit["text"].as_str().unwrap().to_string(), hit["line"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        lines,
        [
            ("pub fn file_outline() -> u32 {".to_string(), 3),
            ("pub fn file_outline_l2() -> u32 {".to_string(), 7),
            ("pub fn outline_file_outline_cached() -> u32 {".to_string(), 11),
        ],
        "{starts}"
    );

    let ends = call(
        &client,
        "code_grep",
        json!({ "pattern": "\\{\\}$", "regex": true, "path": "src/crlf.rs" }),
    )
    .await;
    assert_eq!(ends["total"], 2, "`$` matches before a CRLF terminator too: {ends}");
}

/// A file `code_grep` could not search is counted, so "no hits" is not read as "not there".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_counts_the_files_it_could_not_search() {
    let (dir, client) = started().await;
    let clean = call(&client, "code_grep", json!({ "pattern": "file_outline" })).await;
    assert!(clean.get("skipped_files").is_none(), "{clean}");

    std::fs::write(dir.path().join("src/callers.rs"), b"file_outline \xff\xfe not text\n").unwrap();
    std::fs::remove_file(dir.path().join("worker.rb")).unwrap();

    let body = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline|helper", "regex": true }),
    )
    .await;
    assert_eq!(body["skipped_files"], 2, "one is not UTF-8, one is gone: {body}");
    assert_eq!(
        body["hits"]
            .as_object()
            .map(|files| files.keys().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["src/outline.rs"]),
        "src/outline.rs is still searched: {body}"
    );
}

/// An indexed file swapped for a link out of the repository is not read into a hit.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_does_not_follow_an_indexed_file_swapped_for_a_link_out_of_the_repository() {
    let (dir, client) = started().await;
    let root = dir.path();
    let outside = tempfile::tempdir_in(root.parent().expect("parent")).expect("sibling tempdir");
    std::fs::write(
        outside.path().join("secret.txt"),
        "SECRETTOKEN outside the repository\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("src/callers.rs")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("src/callers.rs")).unwrap();

    let body = call(&client, "code_grep", json!({ "pattern": "SECRETTOKEN" })).await;
    assert_eq!(body["hits"], json!({}), "{body}");
    assert_eq!(body["total"], 0, "{body}");
    assert!(!body.to_string().contains("outside the repository"), "{body}");
    assert_eq!(body["skipped_files"], 1, "{body}");
}

/// A directory swapped for a link leads out of the repository just as a swapped file does. Nothing
/// under it is opened, in the counting pass either: a match count over a file outside the
/// repository answers "does it contain this pattern", one anchored pattern at a time.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_does_not_count_matches_through_a_directory_swapped_for_a_link() {
    let (dir, client) = started().await;
    let root = dir.path();
    let outside = tempfile::tempdir_in(root.parent().expect("parent")).expect("sibling tempdir");
    std::fs::write(
        outside.path().join("callers.rs"),
        "SECRETTOKEN outside the repository\n",
    )
    .unwrap();
    std::fs::remove_dir_all(root.join("src")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("src")).unwrap();

    for pattern in ["SECRETTOKEN", "^SECRET", "outside"] {
        let body = call(&client, "code_grep", json!({ "pattern": pattern, "regex": true })).await;
        assert_eq!(body["hits"], json!({}), "{body}");
        assert_eq!(body["total"], 0, "{pattern}: {body}");
        assert!(body.get("next_cursor").is_none(), "{pattern}: {body}");
        assert_eq!(
            body["skipped_files"], 2,
            "both indexed files under `src/` are reported as not searched: {body}"
        );
        assert!(!body.to_string().contains("outside the repository"), "{body}");
    }

    let elsewhere = call(&client, "code_grep", json!({ "pattern": "def run" })).await;
    assert_eq!(
        elsewhere["total"], 2,
        "files outside the swapped directory: {elsewhere}"
    );
}

/// `code_grep` covers code files. Filters that match only documents are answered with the tool
/// that searches those, not with "no indexed path", which `find_files` would contradict.
#[cfg(feature = "documents")]
#[test]
fn code_grep_says_when_its_filters_match_only_documents() {
    let dir = super::fixture_with(&[]);
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/guide.md"),
        "# Guide\n\nNotes about the harbour and how the harbour is run, long enough to index.\n",
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
        let client = super::serve(root).await;
        let listed = call(&client, "find_files", json!({ "path_prefix": "docs/" })).await;
        assert_eq!(listed["files"][0], "docs/guide.md", "{listed}");

        for arguments in [
            json!({ "pattern": "harbour", "path": "docs/" }),
            json!({ "pattern": "harbour", "language": "markdown" }),
        ] {
            let refused = super::call_err(&client, "code_grep", arguments.clone()).await;
            assert!(
                refused.contains("match only documents (1 indexed, e.g. `docs/guide.md`)")
                    && refused.contains("`docs_search`"),
                "{arguments}: {refused}"
            );
        }
        let _ = client.cancel().await;
    });
}

/// A page cut by `limit` or by the token budget is paging: `next_cursor` says there is more, and
/// `truncated` stays for a cut the caller did not ask for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_cut_is_paging_and_not_truncation() {
    let (_dir, client) = started().await;
    let cut_names = ["truncated_reason", "omitted", "truncation_reason"];
    let assert_paged = |page: &serde_json::Value| {
        assert!(page["next_cursor"].is_string(), "{page}");
        assert!(
            page.get("truncated").is_none_or(|truncated| truncated == false),
            "{page}"
        );
        for name in cut_names {
            assert!(page.get(name).is_none(), "`{name}`: {page}");
        }
    };

    let whole = call(&client, "code_grep", json!({ "pattern": "file_outline" })).await;
    let total = whole["total"].as_u64().expect("total");
    assert!(total > 3, "{whole}");
    assert!(whole.get("next_cursor").is_none(), "{whole}");

    let limited = call(&client, "code_grep", json!({ "pattern": "file_outline", "limit": 2 })).await;
    assert_eq!(limited["total"].as_u64(), Some(total), "{limited}");
    assert_paged(&limited);

    let budgeted = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline", "max_tokens": 1 }),
    )
    .await;
    assert_eq!(budgeted["budgeted"], true, "{budgeted}");
    assert_paged(&budgeted);

    let symbols = call(&client, "code_symbols", json!({ "name": "outline", "limit": 1 })).await;
    assert!(symbols["total"].as_u64().is_some_and(|total| total > 1), "{symbols}");
    assert_paged(&symbols);

    let listed = call(&client, "find_files", json!({ "limit": 1 })).await;
    assert!(listed["total"].as_u64().is_some_and(|total| total > 1), "{listed}");
    assert_paged(&listed);
    let found = call(&client, "find_files", json!({ "query": "rs", "limit": 1 })).await;
    assert!(found["total"].as_u64().is_some_and(|total| total > 1), "{found}");
    assert_paged(&found);
}

/// Every hit is on a line the file has. An anchor also matches at the end of the text, which after
/// a trailing newline is one line past the last: a hit there names a line `code_read` refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_reports_no_hit_past_the_last_line() {
    let (_dir, client) = started().await;

    let blank = call(
        &client,
        "code_grep",
        json!({ "pattern": "^$", "regex": true, "path": "tools.py" }),
    )
    .await;
    assert_eq!(blank["total"], 0, "tools.py has three lines and none is empty: {blank}");

    let ends = call(
        &client,
        "code_grep",
        json!({ "pattern": "$", "regex": true, "path": "tools.py" }),
    )
    .await;
    assert_eq!(ends["total"], 3, "one per line: {ends}");
    let lines: Vec<u64> = rows(&ends["hits"])
        .iter()
        .map(|hit| hit["line"].as_u64().expect("line"))
        .collect();
    assert_eq!(lines, [1, 2, 3], "{ends}");
    for line in lines {
        call(&client, "code_read", json!({ "path": "tools.py", "line": line })).await;
    }
}

const LONE_CR_PY: &str = "a = 1\rb = 2\nc = 3\r\nd = 4\n";

/// A line ends at `\n`, with a `\r` before it counted as part of the ending, in `code_grep` as in
/// `code_read`. A `\r` on its own is text inside its line: no anchor matches at it, so no hit is
/// counted on a line that does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_and_code_read_agree_that_a_lone_carriage_return_ends_no_line() {
    let (_dir, client) = started_with(&[("cr.py", LONE_CR_PY)]).await;

    let read = call(&client, "code_read", json!({ "path": "cr.py", "line": 1 })).await;
    assert_eq!(read["total_lines"], 3, "{read}");
    assert_eq!(read["body"], "a = 1\rb = 2", "{read}");

    let ends = call(
        &client,
        "code_grep",
        json!({ "pattern": "$", "regex": true, "path": "cr.py" }),
    )
    .await;
    let lines: Vec<u64> = rows(&ends["hits"])
        .iter()
        .map(|hit| hit["line"].as_u64().expect("line"))
        .collect();
    assert_eq!(lines, [1, 2, 3], "one end per line: {ends}");

    let starts = call(
        &client,
        "code_grep",
        json!({ "pattern": "^b", "regex": true, "path": "cr.py" }),
    )
    .await;
    assert_eq!(starts["total"], 0, "`b` does not start a line: {starts}");
    let after_crlf = call(
        &client,
        "code_grep",
        json!({ "pattern": "^d = 4$", "regex": true, "path": "cr.py" }),
    )
    .await;
    assert_eq!(rows(&after_crlf["hits"])[0]["line"], 3, "{after_crlf}");
    let before_crlf = call(
        &client,
        "code_grep",
        json!({ "pattern": "c = 3$", "regex": true, "path": "cr.py" }),
    )
    .await;
    assert_eq!(before_crlf["total"], 1, "`$` matches before `\\r\\n`: {before_crlf}");
    let inside = call(&client, "code_grep", json!({ "pattern": "1\rb" , "path": "cr.py" })).await;
    assert_eq!(inside["total"], 1, "a literal still finds the byte: {inside}");
}
