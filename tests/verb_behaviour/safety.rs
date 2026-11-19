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
/// working-tree containment check has nothing to protect there, so it is not applied.
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
    assert_eq!(body["total"], 1, "{body}");
    assert!(body["hits"]["app/[id]/page.tsx"].is_array(), "{body}");

    let glob = call_err(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline", "path": "src/*.rs" }),
    )
    .await;
    assert!(glob.contains("not a glob"), "{glob}");

    let unmatched = call_err(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline", "path": "nowhere/" }),
    )
    .await;
    assert!(
        unmatched.contains("no indexed path contains `nowhere/`"),
        "a plain filter matching no path is refused too: {unmatched}"
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

/// A filter that selects no indexed file is a mistake in the call, not an empty answer: returned
/// as a clean "no hits", a mistyped language or directory reads as "the code does not contain it".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filter_that_selects_no_file_is_refused_with_what_would_match() {
    let (dir, client) = started().await;

    let language = call_err(&client, "code_grep", json!({ "pattern": "fn", "language": "cobol" })).await;
    assert!(language.contains("no indexed file has language `cobol`"), "{language}");
    assert!(
        language.contains("python, ruby, rust"),
        "the languages that would match: {language}"
    );

    let path = call_err(&client, "code_grep", json!({ "pattern": "fn", "path": "no/such/dir/" })).await;
    assert!(path.contains("no indexed path contains `no/such/dir/`"), "{path}");

    let both = call_err(
        &client,
        "code_grep",
        json!({ "pattern": "fn", "path": "src/", "language": "ruby" }),
    )
    .await;
    assert!(
        both.contains("no indexed `ruby` file has a path containing `src/`"),
        "{both}"
    );

    let none = call(
        &client,
        "code_grep",
        json!({ "pattern": "zzz_not_here", "path": "src/" }),
    )
    .await;
    assert_eq!(none["total"], 0, "files were searched and none matched: {none}");

    for arguments in [
        json!({ "language": "cobol" }),
        json!({ "query": "outline", "language": "cobol" }),
    ] {
        let message = call_err(&client, "find_files", arguments).await;
        assert!(message.contains("no indexed file has language `cobol`"), "{message}");
    }
    let prefix = call_err(&client, "find_files", json!({ "path_prefix": "nope/" })).await;
    assert!(prefix.contains("no indexed path starts with `nope/`"), "{prefix}");
    let unmatched = call(
        &client,
        "find_files",
        json!({ "query": "zzzzqqqq", "path_prefix": "src/" }),
    )
    .await;
    assert_eq!(unmatched["files"], json!([]), "{unmatched}");

    let never = call_err(&client, "git_log", json!({ "path": "src/never_existed.rs" })).await;
    assert!(
        never.contains("no commit reachable from HEAD touches `src/never_existed.rs` and no such file exists"),
        "{never}"
    );
    let never_a_directory = call_err(&client, "git_log", json!({ "path": "nowhere/" })).await;
    assert!(
        never_a_directory.contains("no commit reachable from HEAD touches `nowhere/` and no such directory exists"),
        "the directory form is held to the same test: {never_a_directory}"
    );
    let tracked_directory = call(&client, "git_log", json!({ "path": "src/" })).await;
    assert_eq!(
        super::commit_summaries(&tracked_directory["commits"])[0],
        "init",
        "{tracked_directory}"
    );
    std::fs::create_dir(dir.path().join("empty_dir")).unwrap();
    let new_directory = call(&client, "git_log", json!({ "path": "empty_dir/" })).await;
    assert_eq!(
        new_directory["commits"],
        json!([]),
        "a directory with no history yet: {new_directory}"
    );
    std::fs::write(dir.path().join("src/brand_new.rs"), "pub fn fresh() {}\n").unwrap();
    let uncommitted = call(&client, "git_log", json!({ "path": "src/brand_new.rs" })).await;
    assert_eq!(
        uncommitted["commits"],
        json!([]),
        "a file with no history yet: {uncommitted}"
    );
}

/// `limit: 0` asks for an empty page, whose cursor (where one is issued) points back at the same
/// position. Every tool that takes `limit` refuses it, in the same words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_limit_is_refused_by_every_tool_that_pages() {
    let (_dir, client) = started().await;
    for (tool, arguments) in [
        ("code_outline", json!({ "path": "src/outline.rs", "limit": 0 })),
        ("code_symbols", json!({ "name": "file_outline", "limit": 0 })),
        ("code_grep", json!({ "pattern": "fn", "limit": 0 })),
        ("code_search", json!({ "query": "outline a file", "limit": 0 })),
        ("code_refs", json!({ "name": "file_outline", "limit": 0 })),
        (
            "code_refs",
            json!({ "name": "file_outline", "path": "src/outline.rs", "limit": 0 }),
        ),
        (
            "code_refs",
            json!({ "name": "Renderer", "relation": "implementations", "limit": 0 }),
        ),
        (
            "code_refs",
            json!({ "name": "outline", "relation": "importers", "limit": 0 }),
        ),
        ("find_files", json!({ "limit": 0 })),
        ("find_files", json!({ "query": "outline", "limit": 0 })),
        ("docs_search", json!({ "query": "outline", "limit": 0 })),
        ("git_log", json!({ "limit": 0 })),
        ("git_log", json!({ "path": "src/outline.rs", "limit": 0 })),
        ("git_log", json!({ "path": "src/", "limit": 0 })),
        ("git_log", json!({ "query": "init", "limit": 0 })),
        (
            "git_log",
            json!({ "path": "src/outline.rs", "symbol": "file_outline", "limit": 0 }),
        ),
        ("git_blame", json!({ "path": "src/outline.rs", "limit": 0 })),
        ("memory_search", json!({ "limit": 0 })),
        ("memory_search", json!({ "query": "outline", "limit": 0 })),
    ] {
        let message = call_err(&client, tool, arguments.clone()).await;
        assert_eq!(
            message,
            format!("`{tool}`: `limit` must be at least 1; omit it for the default page size."),
            "{arguments}"
        );
    }
}

/// A refusal says what the index holds, which is only an answer once the index is whole. While a
/// scan is unfinished the page comes back with its `notice`, so the caller retries instead of
/// correcting a filter that was right.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filter_refusal_waits_for_a_finished_index() {
    let (dir, client) = started().await;
    let refused = call_err(&client, "code_grep", json!({ "pattern": "fn", "language": "cobol" })).await;
    assert!(refused.contains("no indexed file has language `cobol`"), "{refused}");

    let cache = basemind::store::workspace_cache_dir(dir.path());
    basemind::store::write_status_sidecar(&cache, 5, 5, &basemind::store::ScanHealth::resolving());

    for (tool, arguments, rows) in [
        ("code_grep", json!({ "pattern": "fn", "language": "cobol" }), "hits"),
        ("code_grep", json!({ "pattern": "fn", "path": "no/such/dir/" }), "hits"),
        ("find_files", json!({ "language": "cobol" }), "files"),
        ("find_files", json!({ "path_prefix": "nope/" }), "files"),
        (
            "find_files",
            json!({ "query": "outline", "path_prefix": "nope/" }),
            "files",
        ),
    ] {
        let page = call(&client, tool, arguments.clone()).await;
        let listed = match &page[rows] {
            Value::Array(rows) => rows.len(),
            Value::Object(rows) => rows.len(),
            other => panic!("{tool} {arguments}: `{rows}` is {other}: {page}"),
        };
        assert_eq!(listed, 0, "{tool} {arguments}: {page}");
        assert_eq!(
            page["notice"]["state"], "index_rebuilding",
            "{tool} {arguments}: {page}"
        );
    }
}
