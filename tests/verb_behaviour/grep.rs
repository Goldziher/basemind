//! What `code_grep` matches, which files it reads and what it says about the ones it could not.

use serde_json::json;

use super::{call, call_raw, grep_hits, started, started_with, text_of};

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
        grep_hits(&starts).len(),
        7,
        "3 in outline.rs, 2 in callers.rs, 2 in crlf.rs; the indented `pub fn render` is not one: {starts}"
    );
    assert_eq!(starts["hits"].as_object().map(|files| files.len()), Some(3), "{starts}");
    let lines: Vec<(String, u64)> = grep_hits(&starts)
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
        grep_hits(&ends).len(),
        2,
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
    assert_eq!(grep_hits(&body).len(), 0, "{body}");
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
        assert_eq!(grep_hits(&body).len(), 0, "{pattern}: {body}");
        assert!(body.get("next_cursor").is_none(), "{pattern}: {body}");
        assert_eq!(
            body["skipped_files"], 2,
            "both indexed files under `src/` are reported as not searched: {body}"
        );
        assert!(!body.to_string().contains("outside the repository"), "{body}");
    }

    let elsewhere = call(&client, "code_grep", json!({ "pattern": "def run" })).await;
    assert_eq!(
        grep_hits(&elsewhere).len(),
        2,
        "files outside the swapped directory: {elsewhere}"
    );
}

/// `code_grep` searches the text documents the document tier indexed (markdown, config, data) as
/// well as code, so an exact string in a README is found where `find_files` lists it.
#[cfg(feature = "documents")]
#[test]
fn code_grep_searches_the_text_documents_the_document_tier_indexed() {
    let dir = super::fixture_with(&[]);
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/guide.md"),
        "# Guide\n\nNotes about the harbour and how the harbour is run, long enough to index.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("docs/page.html"),
        "<html><body>\n<p>The quay gate opens at dawn.</p>\n</body></html>\n",
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
        let listed = call(&client, "find_files", json!({ "path_prefix": "docs/" })).await;
        assert_eq!(listed["files"][0], "docs/guide.md", "{listed}");

        for arguments in [
            json!({ "pattern": "harbour is run", "path": "docs/" }),
            json!({ "pattern": "harbour is run", "language": "markdown" }),
        ] {
            let found = call(&client, "code_grep", arguments.clone()).await;
            let hits = grep_hits(&found);
            assert_eq!(hits.len(), 1, "{arguments}: {found}");
            assert_eq!(
                (hits[0]["path"].as_str(), hits[0]["line"].as_u64()),
                (Some("docs/guide.md"), Some(3))
            );
        }
        // HTML is a document now, not code; it stays greppable as a text document.
        let found = call(&client, "code_grep", json!({ "pattern": "quay gate opens" })).await;
        assert_eq!(grep_hits(&found).len(), 1, "{found}");
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
    assert!(
        whole.get("total").is_none(),
        "a page without a cursor lists every hit: {whole}"
    );
    let total = grep_hits(&whole).len() as u64;
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
    assert_eq!(
        grep_hits(&blank).len(),
        0,
        "tools.py has three lines and none is empty: {blank}"
    );

    let ends = call(
        &client,
        "code_grep",
        json!({ "pattern": "$", "regex": true, "path": "tools.py" }),
    )
    .await;
    assert_eq!(grep_hits(&ends).len(), 3, "one per line: {ends}");
    let lines: Vec<u64> = grep_hits(&ends)
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
    let lines: Vec<u64> = grep_hits(&ends)
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
    assert_eq!(grep_hits(&starts).len(), 0, "`b` does not start a line: {starts}");
    let after_crlf = call(
        &client,
        "code_grep",
        json!({ "pattern": "^d = 4$", "regex": true, "path": "cr.py" }),
    )
    .await;
    assert_eq!(grep_hits(&after_crlf)[0]["line"], 3, "{after_crlf}");
    let before_crlf = call(
        &client,
        "code_grep",
        json!({ "pattern": "c = 3$", "regex": true, "path": "cr.py" }),
    )
    .await;
    assert_eq!(
        grep_hits(&before_crlf).len(),
        1,
        "`$` matches before `\\r\\n`: {before_crlf}"
    );
    let inside = call(&client, "code_grep", json!({ "pattern": "1\rb" , "path": "cr.py" })).await;
    assert_eq!(grep_hits(&inside).len(), 1, "a literal still finds the byte: {inside}");
}

/// Two matches on one line keep their columns when a page boundary splits them: a page of one hit
/// still says where on the line its match is, so the pages together tell the two apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_pages_split_on_a_shared_line_keep_their_columns() {
    let (_dir, client) = started_with(&[("src/twice.rs", "pub fn a() {\n    twice_x(); twice_x();\n}\n")]).await;
    let mut columns = Vec::new();
    let mut args = json!({ "pattern": "twice_x(", "limit": 1 });
    for _ in 0..3 {
        let page = call(&client, "code_grep", args.clone()).await;
        for row in grep_hits(&page) {
            assert_eq!(row["line"], 2, "{page}");
            columns.push(row["column"].clone());
        }
        match page["next_cursor"].as_str() {
            Some(cursor) => args["cursor"] = json!(cursor),
            None => break,
        }
    }
    assert_eq!(columns, [json!(5), json!(16)], "each page writes its hit's column");
    let whole = call(&client, "code_grep", json!({ "pattern": "twice_x(" })).await;
    let whole: Vec<_> = grep_hits(&whole).iter().map(|row| row["column"].clone()).collect();
    assert_eq!(whole, columns, "paged and whole agree");
}

const APP_TOML: &str = "[client]\nretry_limit = 3\n";
const NOTES_MD: &str = "# Notes\n\nThe retry_limit setting caps attempts.\n";

/// Config and markdown files are searched too: an exact string in a config file is not a reason to
/// fall back to `rg`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_searches_text_files_outside_code() {
    let (_dir, client) = started_with(&[("cfg/app.toml", APP_TOML), ("docs/notes.md", NOTES_MD)]).await;

    let found = call(&client, "code_grep", json!({ "pattern": "retry_limit" })).await;
    let paths: Vec<String> = grep_hits(&found)
        .iter()
        .map(|hit| format!("{}:{}", hit["path"].as_str().unwrap(), hit["line"]))
        .collect();
    assert_eq!(paths, ["cfg/app.toml:2", "docs/notes.md:3"], "{found}");

    let markdown = call(
        &client,
        "code_grep",
        json!({ "pattern": "retry_limit", "language": "markdown" }),
    )
    .await;
    assert_eq!(grep_hits(&markdown).len(), 1, "{markdown}");
    let one_file = call(
        &client,
        "code_grep",
        json!({ "pattern": "retry_limit", "path": "cfg/app.toml" }),
    )
    .await;
    assert_eq!(grep_hits(&one_file).len(), 1, "{one_file}");
}

/// `ranges` answers each file's hits as read ranges: hits widened by 5 lines, clamped to the file,
/// merged where they touch, ready for `code_read` `ranges`. `context` does not combine with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_ranges_merges_each_files_hits_into_read_ranges() {
    let (_dir, client) = started().await;

    let ranged = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline_l2", "path": "src/", "ranges": true }),
    )
    .await;
    assert!(ranged.get("total").is_none(), "{ranged}");
    assert_eq!(
        ranged["hits"],
        json!({
            "src/outline.rs": { "ranges": [[2, 12], [14, 21]], "hits": 2 },
            "src/callers.rs": { "ranges": [[3, 9]], "hits": 2 },
        }),
        "{ranged}"
    );

    let read = call(
        &client,
        "code_read",
        json!({ "path": "src/outline.rs", "ranges": "2-12,14-21" }),
    )
    .await;
    let spans: Vec<(u64, u64)> = read["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .map(|span| (span["line"].as_u64().unwrap(), span["end_line"].as_u64().unwrap()))
        .collect();
    assert_eq!(spans, [(2, 12), (14, 21)], "{read}");

    let refused = super::call_err(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline_l2", "ranges": true, "context": 1 }),
    )
    .await;
    assert_eq!(
        refused,
        "`code_grep`: `ranges` already widens each hit by 5 lines; drop `context`, or drop `ranges` for hit \
         lines with `context` lines around each."
    );
}

/// `context: N` writes the N lines around each hit as `"<line>- <text>"` rows, each line once where
/// windows overlap (as `rg -C N` merges them), never past the file's last line, under the hit's
/// symbol. More than 10 lines, or a boolean, is refused naming the field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_context_writes_n_lines_around_each_hit_once() {
    let (_dir, client) = started().await;

    let around = call(
        &client,
        "code_grep",
        json!({ "pattern": "-> u32 {", "path": "src/outline.rs", "context": 2 }),
    )
    .await;
    let rows: Vec<&str> = around["hits"]["src/outline.rs"]
        .as_object()
        .expect("rows by symbol")
        .values()
        .flat_map(|rows| rows.as_array().expect("rows"))
        .filter_map(|row| row.as_str())
        .collect();
    let lines: Vec<u32> = rows
        .iter()
        .map(|row| {
            row[..row.find([':', '-']).expect("a line number")]
                .parse()
                .expect("a line")
        })
        .collect();
    assert_eq!(
        lines,
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20],
        "every line within 2 of a hit, once, in order: {around}"
    );
    assert_eq!(
        around["hits"]["src/outline.rs"]["render"],
        json!([
            "16- ",
            "17- impl Renderer {",
            "18:     pub fn render(&self) -> u32 {",
            "19-         file_outline() + file_outline_l2()",
            "20-     }",
        ]),
        "{around}"
    );

    let to_the_end = call(
        &client,
        "code_grep",
        json!({ "pattern": "file_outline() +", "context": 5 }),
    )
    .await;
    let render = to_the_end["hits"]["src/outline.rs"]["render"].as_array().expect("rows");
    assert_eq!(
        render.last(),
        Some(&json!("21- }")),
        "the window stops at the last line, 21: {to_the_end}"
    );
    assert_eq!(render.len(), 8, "lines 14-21: {to_the_end}");

    let wide = super::call_err(&client, "code_grep", json!({ "pattern": "x", "context": 11 })).await;
    assert!(wide.contains("`context` is at most 10 lines (got 11)"), "{wide}");
    let flag = super::call_err(&client, "code_grep", json!({ "pattern": "x", "context": true })).await;
    assert!(flag.contains("`context` must be an integer"), "{flag}");
}

/// The text an agent reads: the other fields as `key: value` lines, then each file's rows under a
/// `# <path>` header and their enclosing symbol's `## <symbol>` header (ADR-0053).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_text_is_rows_under_file_and_symbol_headers() {
    let (_dir, client) = started_with(&[(
        "src/tricky.rs",
        "// a: \"b\", c\nfn tricky_name() {\n    let x = \"tricky_name: \\t\";\n}\n",
    )])
    .await;
    let result = call_raw(&client, "code_grep", json!({ "pattern": "tricky_name", "context": 1 })).await;
    assert_eq!(
        text_of(&result),
        r#"# src/tricky.rs
## tricky_name
1- // a: "b", c
2: fn tricky_name() {
3:     let x = "tricky_name: \t";
4- }"#
    );
}
