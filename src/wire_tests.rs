use proptest::prelude::*;
use serde_json::{Map, Number, Value, json};

use super::*;

/// Strings TOON must quote or escape to read them back as strings.
const TRICKY: &[&str] = &[
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

/// Floats at TOON's number edges: signed zero, integral floats, the `i64` and `u64` bounds, and the
/// extremes of the `f64` range.
const EDGE_FLOATS: &[f64] = &[
    0.0,
    -0.0,
    1.0,
    -1.5,
    0.1,
    1e-7,
    1e300,
    -1e300,
    i64::MAX as f64,
    u64::MAX as f64,
    f64::MIN_POSITIVE,
    5e-324,
    f64::MAX,
];

/// Keys a generated table draws its columns from: bare, and ones a TOON header must quote.
const TABLE_KEYS: &[&str] = &["name", "line", "a b", "a:b", "", "1", "-k", "#k", "k.k", "\"q\""];

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
    let empty = json!({"hits": {}, "scope_files": 0});
    assert_eq!(round_trip("code_grep", &empty), empty.to_string());
}

#[test]
fn toon_tools_round_trip_strings_toon_must_quote() {
    let symbols: Vec<Value> = TRICKY
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
    let files = json!({"files": TRICKY, "next_cursor": "x", "total": 40});
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

#[test]
fn an_empty_first_group_keeps_its_bare_symbol_header() {
    let value = json!({"hits": {"a.rs": {"": []}, "b.rs": {}}});
    assert_eq!(round_trip("code_grep", &value), "# a.rs\n##\n# b.rs");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "TOON could not encode a JSON value")]
fn a_toon_encoder_failure_panics_in_a_debug_build() {
    let deep = (0..300).fold(json!(1), |inner, _| json!({ "a": inner }));
    toon(&deep);
}

#[test]
fn numbers_compare_by_exact_value() {
    assert!(same(&json!(1.0), &json!(1)));
    assert!(same(&json!(-0.0), &json!(0)));
    assert!(same(&json!(u64::MAX), &json!(u64::MAX)));
    assert!(same(&json!(1.5), &json!(1.5)));
    assert!(!same(
        &json!(9_007_199_254_740_993_u64),
        &json!(9_007_199_254_740_992.0)
    ));
    assert!(!same(
        &json!(9_007_199_254_740_992.0),
        &json!(9_007_199_254_740_993_u64)
    ));
    assert!(!same(&json!(1.5), &json!(1)));
    assert!(!same(&json!(f64::MAX), &json!(i64::MAX)));
}

/// Whether `decoded` reads back as `original`, numbers compared by value: TOON writes a number in
/// canonical form (§2), so an integral float such as `1.0` or `-0.0` reads back as the integer.
fn same(original: &Value, decoded: &Value) -> bool {
    match (original, decoded) {
        (Value::Number(a), Value::Number(b)) => same_number(a, b),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, a)| b.get(key).is_some_and(|b| same(a, b)))
        }
        _ => original == decoded,
    }
}

/// Whether two numbers hold the same value, an integer and a float compared without rounding the
/// integer to a float: from 2^53 several integers round to one float.
fn same_number(a: &Number, b: &Number) -> bool {
    match (integer(a), integer(b)) {
        (Some(a), Some(b)) => a == b,
        (Some(int), None) => b.as_f64().is_some_and(|float| float_is(float, int)),
        (None, Some(int)) => a.as_f64().is_some_and(|float| float_is(float, int)),
        (None, None) => a.as_f64() == b.as_f64(),
    }
}

fn integer(number: &Number) -> Option<i128> {
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

/// Whether `float` is exactly `int`. A float at or past 2^127 saturates the cast to an `i128` bound,
/// which no `i64` or `u64` reaches.
fn float_is(float: f64, int: i128) -> bool {
    float.fract() == 0.0 && float as i128 == int
}

/// Any string, control characters and line breaks included, or one TOON must quote.
fn text() -> impl Strategy<Value = String> {
    prop_oneof!["(?s).{0,16}", prop::sample::select(TRICKY).prop_map(str::to_owned)]
}

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f64>().prop_map(Value::from),
        prop::sample::select(EDGE_FLOATS).prop_map(Value::from),
        text().prop_map(Value::String),
    ]
}

/// An array of flat objects over one key set, which TOON writes as a table; a ragged row drops the
/// last key, which makes TOON fall back to list items.
fn table() -> impl Strategy<Value = Value> {
    (
        prop::sample::subsequence(TABLE_KEYS, 1..=TABLE_KEYS.len()),
        prop::collection::vec((prop::collection::vec(leaf(), TABLE_KEYS.len()), any::<bool>()), 0..5),
    )
        .prop_map(|(keys, rows)| {
            let width = |ragged: bool| keys.len() - usize::from(ragged);
            let rows = rows.into_iter().map(|(cells, ragged)| {
                let row = keys.iter().zip(cells).take(width(ragged));
                Value::Object(row.map(|(key, cell)| ((*key).to_string(), cell)).collect())
            });
            Value::Array(rows.collect())
        })
}

fn fields(field: impl Strategy<Value = Value>) -> impl Strategy<Value = Map<String, Value>> {
    prop::collection::vec((text(), field), 0..6).prop_map(|pairs| pairs.into_iter().collect::<Map<String, Value>>())
}

/// Any JSON value: scalars, nested arrays and objects, and tables.
fn value() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(4, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            fields(inner).prop_map(Value::Object),
            table(),
        ]
    })
}

/// A tool answer: an object of any values.
fn answer() -> impl Strategy<Value = Map<String, Value>> {
    fields(value())
}

/// A `code_grep` `hits` map: paths to symbol groups to numbered rows, the shape the row block carries.
fn hits() -> impl Strategy<Value = Value> {
    let rows = prop::collection::vec("[0-9][^\r\n]{0,16}".prop_map(Value::String), 0..4).prop_map(Value::Array);
    let groups =
        prop::collection::vec((text(), rows), 0..4).prop_map(|groups| Value::Object(groups.into_iter().collect()));
    prop::collection::vec((text(), groups), 0..4).prop_map(|files| Value::Object(files.into_iter().collect()))
}

fn reads_back(tool: &str, value: &Value) -> Result<(), TestCaseError> {
    let text = encode(tool, value);
    let decoded = decode(&text).map_err(|error| TestCaseError::fail(format!("{tool}: {error}\n{text}")))?;
    prop_assert!(
        same(value, &decoded),
        "{tool} round trip:\n{text}\nread back {decoded}\nfrom {value}"
    );
    Ok(())
}

proptest! {
    #[test]
    fn every_json_answer_reads_back_exactly(body in answer()) {
        let value = Value::Object(body);
        let text = encode("code_symbols", &value);
        prop_assert_eq!(&text, &value.to_string());
        prop_assert_eq!(decode(&text), Ok(value));
    }

    #[test]
    fn every_toon_answer_reads_back(body in answer()) {
        let value = Value::Object(body);
        reads_back("code_outline", &value)?;
        reads_back("git_diff", &value)?;
    }

    #[test]
    fn every_find_files_list_reads_back(mut body in answer(), files in prop::collection::vec(value(), 0..6)) {
        body.insert("files".into(), Value::Array(files));
        reads_back("find_files", &Value::Object(body))?;
    }

    #[test]
    fn every_grep_answer_reads_back(mut body in answer(), grep_hits in hits()) {
        body.insert(HITS.into(), grep_hits);
        reads_back("code_grep", &Value::Object(body))?;
    }
}
