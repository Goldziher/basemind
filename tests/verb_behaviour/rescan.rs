//! `index_rescan` with named paths: a directory is rescanned file by file, and a path that cannot
//! be a rescan target is refused rather than answered with an empty success.

use serde_json::json;

use super::{call, call_err, started, symbol_rows};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_rescan_of_a_directory_rescans_and_prunes_the_files_under_it() {
    let (dir, client) = started().await;
    let outline = dir.path().join("src/outline.rs");
    let source = std::fs::read_to_string(&outline).unwrap();
    std::fs::write(
        &outline,
        format!("{source}\npub fn added_after_scan() -> u32 {{\n    4\n}}\n"),
    )
    .unwrap();
    std::fs::remove_file(dir.path().join("src/callers.rs")).unwrap();

    let report = call(&client, "index_rescan", json!({ "paths": ["src/"] })).await;
    assert_eq!(report["updated"], 1, "{report}");
    assert_eq!(report["removed"], 1, "{report}");

    let found = call(&client, "code_symbols", json!({ "name": "added_after_scan" })).await;
    assert_eq!(
        symbol_rows(&found),
        ["src/outline.rs:23: pub fn added_after_scan() -> u32"],
        "{found}"
    );
    let fresh = call(&client, "code_outline", json!({ "path": "src/outline.rs" })).await;
    assert!(fresh.get("stale").is_none(), "{fresh}");
    let gone = call(&client, "code_symbols", json!({ "name": "first" })).await;
    assert_eq!(gone["results"], json!([]), "src/callers.rs left the index: {gone}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_rescan_of_a_deleted_directory_prunes_everything_under_it() {
    let (dir, client) = started().await;
    std::fs::remove_dir_all(dir.path().join("src")).unwrap();

    let report = call(&client, "index_rescan", json!({ "paths": ["src"] })).await;
    assert_eq!(report["removed"], 2, "{report}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_rescan_refuses_a_path_it_cannot_rescan() {
    let (_dir, client) = started().await;
    let outside = tempfile::tempdir().unwrap();
    let absolute = outside.path().join("elsewhere.rs");
    std::fs::write(&absolute, "fn x() {}\n").unwrap();

    for (path, says) in [
        ("src/typo.rs", "does not exist and is not in the index"),
        (".", "names the root itself"),
        ("../outside.rs", "outside the repository"),
        (absolute.to_str().unwrap(), "outside the repository"),
    ] {
        let message = call_err(&client, "index_rescan", json!({ "paths": [path] })).await;
        assert!(
            message.contains(says) && message.contains("find_files"),
            "{path}: {message}"
        );
    }
}
