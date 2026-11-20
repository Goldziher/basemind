use serde_json::{Value, json};

use super::*;

fn round_trip(tool: &str, value: &Value) -> String {
    let text = encode(tool, value);
    let back = decode(&text).unwrap_or_else(|e| panic!("{tool}: {e}\n{text}"));
    assert_eq!(&back, value, "{tool} round trip:\n{text}");
    let order = |v: &Value| {
        v.get(HITS)
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
    };
    assert_eq!(order(&back), order(value), "file order is rank order");
    text
}

#[test]
fn grep_rows_are_a_plain_block_after_the_toon_lines() {
    let value = json!({
        "total": 7,
        "hits": {
            "src/a.rs": {"": ["1: use x;"], "main": ["3- // before", "4: let a = b;", "5- "]},
            "src/b.rs": {"run": ["10:5,9: f(x) + f(y)"], "": ["20: tail"]},
        },
        "next_cursor": "abc",
    });
    let text = round_trip("code_grep", &value);
    assert_eq!(
        text,
        "total: 7\nnext_cursor: abc\n# src/a.rs\n1: use x;\n## main\n3- // before\n4: let a = b;\n5- \n\
         # src/b.rs\n## run\n10:5,9: f(x) + f(y)\n##\n20: tail"
    );
}

#[test]
fn grep_rows_with_delimiters_quotes_tabs_and_unicode_round_trip() {
    let value = json!({
        "budgeted": true,
        "hits": {
            "dir with space/a:b.rs": {"": ["1: a: b, c - d", "2- \t\"quoted\" \\ back"]},
            "2024-notes.md": {"": ["3: # heading", "4: ## not a header"]},
            "#hash.md": {"## odd": ["5:   leading spaces", "6: trailing  "]},
            "\"quoted\".rs": {" padded ": ["7: ünïcödé 文字 🚀"]},
            " leading.rs": {"x\ty": ["8: -1"], "": ["9: 12: looks like a row"]},
            "ünï/文件.rs": {"Struct::method": ["10- ", "11: {\"json\": [1, 2]}"]},
            "new\nline.rs": {"multi\nline": ["12: x"]},
        },
        "stale": ["dir with space/a:b.rs", "a,b.rs", "-dash.rs"],
        "skipped_files": 2,
        "notice": {"kind": "warming_up", "message": "index: loading, retry"},
        "next_cursor": "c-1:2",
    });
    round_trip("code_grep", &value);
}

#[test]
fn grep_answers_the_block_cannot_carry_stay_json() {
    let carriage = json!({"hits": {"a.rs": {"": ["1: a\rb"]}}});
    assert_eq!(round_trip("code_grep", &carriage), carriage.to_string());
    let unnumbered = json!({"hits": {"a.rs": {"": ["x: a"]}}});
    assert_eq!(round_trip("code_grep", &unnumbered), unnumbered.to_string());
    let ranges = json!({"hits": {"a.rs": {"ranges": [[1, 10]], "hits": 2}}});
    assert_eq!(round_trip("code_grep", &ranges), ranges.to_string());
    let empty = json!({"hits": {}, "scope_files": 0});
    assert_eq!(round_trip("code_grep", &empty), empty.to_string());
}

#[test]
fn toon_tools_round_trip_strings_toon_must_quote() {
    let tricky = [
        "",
        " lead",
        "trail ",
        "-dash",
        "#hash",
        "123",
        "1.5",
        "true",
        "null",
        "a,b",
        "a:b",
        "a\"b",
        "a\\b",
        "tab\there",
        "line\nbreak",
        "[x]",
        "{y}",
        "ünï 文字",
        "fn f<'a>(x: &'a str) -> Option<&'a str>",
    ];
    let symbols: Vec<Value> = tricky
        .iter()
        .enumerate()
        .map(|(i, s)| json!({"name": s, "kind": "function", "line": i + 1, "end_line": i + 2, "signature": s}))
        .collect();
    let outline = json!({"path": "dir with space/a:b.rs", "language": "rust", "total": 3, "symbols": symbols});
    let text = round_trip("code_outline", &outline);
    assert!(
        text.contains("symbols[19]{name,kind,line,end_line,signature}:"),
        "{text}"
    );
    let ragged =
        json!({"path": "a.rs", "symbols": [{"name": "a", "line": 1}, {"name": "b", "line": 2, "signature": "fn b()"}]});
    round_trip("code_outline", &ragged);
    let files = json!({"files": tricky, "next_cursor": "x", "total": 40});
    round_trip("find_files", &files);
    let diff = json!({"path": "a.rs", "rev": "0123abc", "added": [{"name": "-x", "kind": "fn"}], "removed": [], "common": 3,
                      "hunks": [{"old_line_start": 1, "old_line_count": 0, "new_line_start": 1, "new_line_count": 2,
                                 "lines": ["+a: b", "-\"c\"", " d"]}]});
    round_trip("git_diff", &diff);
}

#[test]
fn json_tools_keep_json_text() {
    let value = json!({"results": ["src/a.rs:3: fn a()"], "next_cursor": "c"});
    for tool in [
        "code_symbols",
        "code_refs",
        "git_log",
        "git_blame",
        "code_read",
        "index_status",
    ] {
        assert_eq!(round_trip(tool, &value), value.to_string());
    }
    let sized = json!({"files": {"src/a.rs": 3000}});
    assert_eq!(round_trip("find_files", &sized), sized.to_string());
}

#[test]
fn a_malformed_block_is_an_error_naming_the_line() {
    assert!(decode("# a.rs\n1: x\nnot a row").unwrap_err().contains("not a row"));
    assert!(decode("total: 1\n# a.rs\n## \"unterminated\n1: x").is_err());
    assert!(decode("hits: 3\n# a.rs\n1: x").unwrap_err().contains("hits"));
}
