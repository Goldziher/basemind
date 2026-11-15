//! What `code_grep` matches, which files it reads and what it says about the ones it could not.

use serde_json::json;

use super::{call, started, started_with};

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
        starts["total_matches"], 7,
        "3 in outline.rs, 2 in callers.rs, 2 in crlf.rs; the indented `pub fn render` is not one: {starts}"
    );
    assert_eq!(starts["total_files_matched"], 3, "{starts}");
    let lines: Vec<(String, u64)> = starts["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .filter(|hit| hit["path"] == "src/outline.rs")
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
    assert_eq!(
        ends["total_matches"], 2,
        "`$` matches before a CRLF terminator too: {ends}"
    );
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
        body["total_files_matched"], 1,
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
    assert_eq!(body["hits"], json!([]), "{body}");
    assert_eq!(body["total_matches"], 0, "{body}");
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
        assert_eq!(body["hits"], json!([]), "{body}");
        assert_eq!(body["total_matches"], 0, "{pattern}: {body}");
        assert_eq!(body["total_files_matched"], 0, "{pattern}: {body}");
        assert!(body.get("next_cursor").is_none(), "{pattern}: {body}");
        assert_eq!(
            body["skipped_files"], 2,
            "both indexed files under `src/` are reported as not searched: {body}"
        );
        assert!(!body.to_string().contains("outside the repository"), "{body}");
    }

    let elsewhere = call(&client, "code_grep", json!({ "pattern": "def run" })).await;
    assert_eq!(
        elsewhere["total_matches"], 2,
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
        assert_eq!(listed["files"][0]["path"], "docs/guide.md", "{listed}");

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

/// A cut page names what cut it and how many matches it left out, in the words every tool uses:
/// `truncated` + `truncated_reason` for a hard bound, `budgeted` for the token budget, `omitted`
/// for the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_names_what_cut_a_page_and_how_much_it_left_out() {
    let (_dir, client) = started().await;

    let whole = call(&client, "code_grep", json!({ "pattern": "file_outline" })).await;
    let total = whole["total_matches"].as_u64().expect("total_matches");
    assert!(total > 3, "{whole}");
    assert_eq!(whole["truncated"], false, "{whole}");
    assert!(
        whole.get("truncated_reason").is_none() && whole.get("omitted").is_none(),
        "{whole}"
    );

    let limited = call(&client, "code_grep", json!({ "pattern": "file_outline", "limit": 2 })).await;
    assert_eq!(limited["truncated"], true, "{limited}");
    assert_eq!(limited["truncated_reason"], "limit", "{limited}");
    assert_eq!(limited["omitted"].as_u64(), Some(total - 2), "{limited}");
    assert!(limited["next_cursor"].is_string(), "{limited}");
    assert!(limited.get("truncation_reason").is_none(), "{limited}");

    let budgeted = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline", "max_tokens": 1 }),
    )
    .await;
    assert_eq!(budgeted["budgeted"], true, "{budgeted}");
    assert_eq!(
        budgeted["truncated"], false,
        "the budget is not a hard bound: {budgeted}"
    );
    assert_eq!(budgeted["omitted"].as_u64(), Some(total - 1), "{budgeted}");
    assert!(budgeted["next_cursor"].is_string(), "{budgeted}");
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
    assert_eq!(
        blank["total_matches"], 0,
        "tools.py has three lines and none is empty: {blank}"
    );

    let ends = call(
        &client,
        "code_grep",
        json!({ "pattern": "$", "regex": true, "path": "tools.py" }),
    )
    .await;
    assert_eq!(ends["total_matches"], 3, "one per line: {ends}");
    let lines: Vec<u64> = ends["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["line"].as_u64().expect("line"))
        .collect();
    assert_eq!(lines, [1, 2, 3], "{ends}");
    for line in lines {
        call(&client, "code_read", json!({ "path": "tools.py", "line": line })).await;
    }
}
