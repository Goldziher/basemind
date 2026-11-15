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

/// A directory swapped for a link is caught where content is returned. The counting pass checks
/// only whether the file itself is a link, so the match is counted, but its text never leaves.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_returns_no_text_through_a_directory_swapped_for_a_link() {
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

    let body = call(&client, "code_grep", json!({ "pattern": "SECRETTOKEN" })).await;
    assert_eq!(body["hits"], json!([]), "{body}");
    assert!(!body.to_string().contains("outside the repository"), "{body}");
}
