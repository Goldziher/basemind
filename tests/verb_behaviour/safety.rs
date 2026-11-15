//! A caller-supplied path never reads outside the repository, and `code_grep`'s filters mean what
//! they say.

use serde_json::json;

use super::{call, call_err, started, started_with};

const SECRET: &str = "SECRETTOKEN outside the repository\n";

/// A directory beside the fixture holding one file, and that file's path relative to `root`.
fn outside_file(root: &std::path::Path) -> (tempfile::TempDir, String) {
    let parent = root.parent().expect("the fixture has a parent");
    let outside = tempfile::tempdir_in(parent).expect("sibling tempdir");
    std::fs::write(outside.path().join("secret.txt"), SECRET).unwrap();
    let name = outside.path().file_name().unwrap().to_str().unwrap();
    let relative = format!("../{name}/secret.txt");
    (outside, relative)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_refuses_a_parent_directory_path() {
    let (dir, client) = started().await;
    let (_outside, relative) = outside_file(dir.path());

    let message = call_err(&client, "git_diff", json!({ "path": relative })).await;
    assert!(!message.contains("SECRETTOKEN"), "{message}");
    assert!(message.contains("outside this repository"), "{message}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_refuses_a_symlink_that_leaves_the_repository() {
    let (dir, client) = started().await;
    let (outside, _relative) = outside_file(dir.path());
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link.txt")).unwrap();

    let message = call_err(&client, "git_diff", json!({ "path": "link.txt" })).await;
    assert!(!message.contains("SECRETTOKEN"), "{message}");
    assert!(message.contains("outside this repository"), "{message}");
}

/// Between two revisions only blobs are read, and a tracked symlink's blob is its target path. The
/// working-tree containment check has nothing to protect there and used to refuse the diff.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_between_revisions_reads_a_tracked_symlink_as_git_stores_it() {
    let (dir, client) = started().await;
    let root = dir.path();
    let (outside, _relative) = outside_file(root);
    std::os::unix::fs::symlink("src/outline.rs", root.join("link.txt")).unwrap();
    super::git(root, &["add", "-A"]);
    super::git(root, &["commit", "-qm", "link inside"]);
    std::fs::remove_file(root.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link.txt")).unwrap();
    super::git(root, &["add", "-A"]);
    super::git(root, &["commit", "-qm", "link outside"]);

    let diff = call(
        &client,
        "git_diff",
        json!({ "path": "link.txt", "from": "HEAD~1", "to": "HEAD" }),
    )
    .await;
    let rendered = diff.to_string();
    assert!(rendered.contains("src/outline.rs"), "the old target path: {diff}");
    assert!(rendered.contains("secret.txt"), "the new target path: {diff}");
    assert!(!rendered.contains("SECRETTOKEN"), "{diff}");

    let worktree = call_err(&client, "git_diff", json!({ "path": "link.txt" })).await;
    assert!(worktree.contains("outside this repository"), "{worktree}");
    assert!(!worktree.contains("SECRETTOKEN"), "{worktree}");

    let climbing = call_err(
        &client,
        "git_diff",
        json!({ "path": "src/../../link.txt", "from": "HEAD~1", "to": "HEAD" }),
    )
    .await;
    assert!(climbing.contains("outside this repository"), "{climbing}");
}

/// The link is an intermediate directory, so the path's last component is an ordinary file name.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_directory_does_not_lead_out_of_the_repository() {
    let (dir, client) = started().await;
    let (outside, _relative) = outside_file(dir.path());
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linkdir")).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("linkdir/secret.txt")).unwrap(),
        SECRET,
        "the link resolves, so only the containment check stands between a caller and the file"
    );

    for (tool, arguments) in [
        ("git_diff", json!({ "path": "linkdir/secret.txt" })),
        ("code_read", json!({ "path": "linkdir/secret.txt", "line": 1 })),
        ("code_read", json!({ "path": "linkdir/secret.txt", "symbol": "x" })),
        ("code_outline", json!({ "path": "linkdir/secret.txt" })),
        (
            "code_refs",
            json!({ "name": "SECRETTOKEN", "relation": "definition", "path": "linkdir/secret.txt", "line": 1 }),
        ),
    ] {
        let message = call_err(&client, tool, arguments).await;
        assert!(!message.contains("SECRETTOKEN"), "{tool}: {message}");
        assert!(message.contains("outside this repository"), "{tool}: {message}");
    }
}

/// Relation `definition` read the file before consulting the index, so its two different errors
/// ("not on that line" / "exists but is not indexed") told a caller what a file outside the
/// repository contained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_definition_cannot_probe_a_file_outside_the_repository() {
    let (dir, client) = started().await;
    let (outside, relative) = outside_file(dir.path());
    let absolute = outside.path().join("secret.txt").to_string_lossy().into_owned();

    for path in [relative.as_str(), absolute.as_str()] {
        let present = call_err(
            &client,
            "code_refs",
            json!({ "name": "SECRETTOKEN", "relation": "definition", "path": path, "line": 1 }),
        )
        .await;
        let absent = call_err(
            &client,
            "code_refs",
            json!({ "name": "NOSUCHTOKEN", "relation": "definition", "path": path, "line": 1 }),
        )
        .await;
        assert_eq!(present, absent, "the answer must not depend on the file's contents");
        assert!(present.contains("outside this repository"), "{present}");
    }
}

/// A file inside the repository that is not indexed is refused before it is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_definition_consults_the_index_before_reading_the_file() {
    let (dir, client) = started().await;
    std::fs::write(dir.path().join("unindexed.py"), "total = 1\n").unwrap();

    let present = call_err(
        &client,
        "code_refs",
        json!({ "name": "total", "relation": "definition", "path": "unindexed.py", "line": 1 }),
    )
    .await;
    let absent = call_err(
        &client,
        "code_refs",
        json!({ "name": "missing", "relation": "definition", "path": "unindexed.py", "line": 1 }),
    )
    .await;
    assert_eq!(present, absent);
    assert!(present.contains("not indexed"), "{present}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_read_and_outline_refuse_a_path_outside_the_repository() {
    let (dir, client) = started().await;
    let (_outside, relative) = outside_file(dir.path());

    for (tool, arguments) in [
        ("code_read", json!({ "path": relative, "line": 1 })),
        ("code_read", json!({ "path": relative, "symbol": "x" })),
        ("code_outline", json!({ "path": relative })),
    ] {
        let message = call_err(&client, tool, arguments).await;
        assert!(!message.contains("SECRETTOKEN"), "{tool}: {message}");
        assert!(message.contains("outside this repository"), "{tool}: {message}");
    }
}

const PAGE_TSX: &str = "export function Page() {\n  return \"bracket-needle\";\n}\n";

/// `path` is a substring, so a path that happens to contain glob characters is searchable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_path_is_a_literal_substring_even_with_glob_characters() {
    let (_dir, client) = started_with(&[("app/[id]/page.tsx", PAGE_TSX)]).await;

    let body = call(
        &client,
        "code_grep",
        json!({ "pattern": "bracket-needle", "path": "app/[id]/" }),
    )
    .await;
    assert_eq!(body["total_matches"], 1, "{body}");
    assert_eq!(body["hits"][0]["path"], "app/[id]/page.tsx", "{body}");

    let glob = call_err(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline", "path": "src/*.rs" }),
    )
    .await;
    assert!(glob.contains("not a glob"), "{glob}");

    let unmatched = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline", "path": "nowhere/" }),
    )
    .await;
    assert_eq!(
        unmatched["total_matches"], 0,
        "a plain filter matching nothing is an empty result: {unmatched}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_refuses_an_empty_pattern() {
    let (_dir, client) = started().await;

    let message = call_err(&client, "code_grep", json!({ "pattern": "" })).await;
    assert!(message.contains("`pattern` is empty"), "{message}");
}

const LOOKUP_PY: &str = "\
def lookup(budget, get):
    return budget.pop(get)
";

/// `get` occurs inside `budget` first. The default column is the first whole-identifier occurrence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_definition_finds_the_name_on_an_identifier_boundary() {
    let (_dir, client) = started_with(&[("lookup.py", LOOKUP_PY)]).await;

    let body = call(
        &client,
        "code_refs",
        json!({ "name": "get", "relation": "definition", "path": "lookup.py", "line": 2 }),
    )
    .await;
    assert_eq!(body["column"], 23, "`    return budget.pop(get)`: {body}");
    assert_eq!(body["definition"]["line"], 1, "{body}");
    assert_eq!(body["definition"]["name"], "get", "{body}");
}
