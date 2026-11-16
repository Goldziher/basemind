//! Turn a tool response (parsed JSON) into the ranked list of [`Item`]s the scorer compares to
//! gold. The item shape per mode:
//!
//! | mode | item |
//! |---|---|
//! | symbols | `path:line` of each definition (1-based) |
//! | outline | `args.path:line` of each symbol in the file |
//! | references, callers | `path:line` of each call site |
//! | grep | `path:line` of each match |
//! | find, dependents | `path` |
//! | docs | `path` of each hit chunk |
//! | git_search, git_path | the commit `sha` |
//! | git_symbol | the `sha` of each history entry |
//! | implementations | `path:line` of each impl |
//! | read | `path:line` for every line of the returned span |
//! | blame, blame_symbol | `sha:line` for every line of each hunk |
//! | diff | `+path:line` per added line, `-path:line` per removed line |
//! | diff_structural | `+name` per added symbol, `-name` per removed one |

use serde_json::Value;

use super::score::Item;
use super::task::EvalMode;

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn u(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn located(entries: &[Value], line: impl Fn(&Value) -> Option<u32>) -> Vec<Item> {
    entries
        .iter()
        .filter_map(|e| {
            Some(Item {
                path: s(e, "path")?,
                line: line(e),
            })
        })
        .collect()
}

/// Items for `mode` from response `body`; `args` supplies the file for `outline`.
pub fn extract_items(mode: EvalMode, args: &Value, body: &Value) -> Vec<Item> {
    match mode {
        EvalMode::Symbols => located(array(body, "results"), |e| u(e, "line")),
        EvalMode::Outline => {
            let path = s(body, "path").or_else(|| s(args, "path")).unwrap_or_default();
            array(body, "symbols")
                .iter()
                .map(|e| Item {
                    path: path.clone(),
                    line: u(e, "line"),
                })
                .collect()
        }
        EvalMode::References | EvalMode::Callers => located(array(body, "hits"), |e| u(e, "line")),
        EvalMode::Grep => located(array(body, "hits"), |e| u(e, "line")),
        EvalMode::Find => located(array(body, "files"), |_| None),
        EvalMode::Dependents => array(body, "paths")
            .iter()
            .filter_map(|p| p.as_str())
            .map(|p| Item {
                path: p.to_string(),
                line: None,
            })
            .collect(),
        EvalMode::Docs => located(array(body, "hits"), |_| None),
        EvalMode::GitSearch | EvalMode::GitPath => shas(array(body, "commits")),
        EvalMode::GitSymbol => shas(array(body, "history")),
        EvalMode::Implementations => located(array(body, "hits"), |e| u(e, "line")),
        EvalMode::Read => {
            let path = s(body, "path").unwrap_or_default();
            span(&path, u(body, "line"), u(body, "end_line"))
        }
        EvalMode::Blame | EvalMode::BlameSymbol => array(body, "hunks")
            .iter()
            .flat_map(|h| span(&s(h, "commit_sha").unwrap_or_default(), u(h, "line"), u(h, "end_line")))
            .collect(),
        EvalMode::Diff => {
            let path = s(body, "path").unwrap_or_default();
            array(body, "hunks")
                .iter()
                .flat_map(|h| {
                    let side = |sign: char, start: &str, count: &str| {
                        let (start, count) = (u(h, start).unwrap_or(0), u(h, count).unwrap_or(0));
                        let first = (count > 0).then_some(start);
                        span(&format!("{sign}{path}"), first, first.map(|f| f + count - 1))
                    };
                    let mut v = side('-', "old_line_start", "old_line_count");
                    v.extend(side('+', "new_line_start", "new_line_count"));
                    v
                })
                .collect()
        }
        EvalMode::DiffStructural => {
            let named = |key: &str, sign: char| {
                array(body, key)
                    .iter()
                    .filter_map(|e| e.as_str().map(str::to_string).or_else(|| s(e, "name")))
                    .map(|n| Item {
                        path: format!("{sign}{n}"),
                        line: None,
                    })
                    .collect::<Vec<_>>()
            };
            let mut v = named("added", '+');
            v.extend(named("removed", '-'));
            v
        }
    }
}

/// `code_grep` hits as `(path:line, attached symbol name)`.
pub fn grep_annotations(body: &Value) -> Vec<(String, Option<String>)> {
    array(body, "hits")
        .iter()
        .filter_map(|h| {
            let key = format!("{}:{}", s(h, "path")?, u(h, "line")?);
            Some((key, h.get("symbol").and_then(|sym| s(sym, "name"))))
        })
        .collect()
}

/// Share of the annotated gold lines among `hits` whose attached symbol has the expected name;
/// `None` when no returned hit is an annotated gold line.
pub fn annotation_accuracy(
    hits: &[(String, Option<String>)],
    expected: &std::collections::BTreeMap<String, String>,
) -> Option<f64> {
    let judged: Vec<bool> = hits
        .iter()
        .filter_map(|(key, got)| expected.get(key).map(|want| got.as_deref() == Some(want.as_str())))
        .collect();
    (!judged.is_empty()).then(|| judged.iter().filter(|ok| **ok).count() as f64 / judged.len() as f64)
}

fn shas(entries: &[Value]) -> Vec<Item> {
    entries
        .iter()
        .filter_map(|c| s(c, "sha"))
        .map(|sha| Item { path: sha, line: None })
        .collect()
}

/// `key:line` for every line of `first..=last` (`last` defaults to `first`).
fn span(key: &str, first: Option<u32>, last: Option<u32>) -> Vec<Item> {
    let Some(first) = first else { return Vec::new() };
    let last = last.unwrap_or(first).max(first);
    (first..=last)
        .map(|line| Item {
            path: key.to_string(),
            line: Some(line),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn maps_each_shape_with_one_based_lines() {
        let sym = extract_items(
            EvalMode::Symbols,
            &Value::Null,
            &json!({"results":[{"path":"a.py","name":"f","line":1},{"path":"b.py","line":10}]}),
        );
        assert_eq!(
            sym[0],
            Item {
                path: "a.py".into(),
                line: Some(1)
            }
        );
        assert_eq!(sym[1].line, Some(10));

        let outline = extract_items(
            EvalMode::Outline,
            &json!({"path":"a.py"}),
            &json!({"symbols":[{"name":"f","line":5}]}),
        );
        assert_eq!(
            outline,
            vec![Item {
                path: "a.py".into(),
                line: Some(5)
            }]
        );

        let refs = extract_items(
            EvalMode::References,
            &Value::Null,
            &json!({"hits":[{"path":"x","line":7}]}),
        );
        assert_eq!(refs[0].line, Some(7));
        let grep = extract_items(EvalMode::Grep, &Value::Null, &json!({"hits":[{"path":"x","line":3}]}));
        assert_eq!(grep[0].line, Some(3));
        let find = extract_items(
            EvalMode::Find,
            &Value::Null,
            &json!({"files":[{"path":"p/q.py","score":1}]}),
        );
        assert_eq!(
            find[0],
            Item {
                path: "p/q.py".into(),
                line: None
            }
        );
        let dep = extract_items(EvalMode::Dependents, &Value::Null, &json!({"paths":["m.py"]}));
        assert_eq!(dep[0].path, "m.py");
        let git = extract_items(EvalMode::GitSearch, &Value::Null, &json!({"commits":[{"sha":"abc"}]}));
        assert_eq!(git[0].path, "abc");
        assert!(extract_items(EvalMode::Docs, &Value::Null, &json!({})).is_empty());
    }

    fn strs(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i.line {
                Some(l) => format!("{}:{l}", i.path),
                None => i.path.clone(),
            })
            .collect()
    }

    #[test]
    fn scores_the_symbol_attached_to_each_annotated_grep_hit() {
        let body = json!({"hits":[
            {"path":"a.rs","line":3,"symbol":{"name":"spin","kind":"function"}},
            {"path":"a.rs","line":9,"symbol":{"name":"Wrong","kind":"struct"}},
            {"path":"a.rs","line":12},
            {"path":"b.rs","line":1,"symbol":{"name":"x","kind":"function"}}
        ]});
        let hits = grep_annotations(&body);
        let expected: std::collections::BTreeMap<String, String> =
            [("a.rs:3", "spin"), ("a.rs:9", "Widget"), ("a.rs:12", "run")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        // One right, one wrong name, one missing symbol; b.rs:1 has no expectation.
        let acc = annotation_accuracy(&hits, &expected).unwrap();
        assert!((acc - 1.0 / 3.0).abs() < 1e-9, "{acc}");
        assert_eq!(annotation_accuracy(&hits[3..], &expected), None);
    }

    #[test]
    fn maps_git_read_and_impl_shapes() {
        let hist = extract_items(EvalMode::GitSymbol, &Value::Null, &json!({"history":[{"sha":"s1"}]}));
        assert_eq!(strs(&hist), vec!["s1"]);
        let path = extract_items(EvalMode::GitPath, &Value::Null, &json!({"commits":[{"sha":"s2"}]}));
        assert_eq!(strs(&path), vec!["s2"]);
        let read = extract_items(
            EvalMode::Read,
            &Value::Null,
            &json!({"path":"a.rs","line":4,"end_line":6,"body":"x"}),
        );
        assert_eq!(strs(&read), vec!["a.rs:4", "a.rs:5", "a.rs:6"]);
        let blame = extract_items(
            EvalMode::BlameSymbol,
            &Value::Null,
            &json!({"hunks":[{"commit_sha":"c1","line":2,"end_line":3},{"commit_sha":"c2","line":9}]}),
        );
        assert_eq!(strs(&blame), vec!["c1:2", "c1:3", "c2:9"]);
        let diff = extract_items(
            EvalMode::Diff,
            &Value::Null,
            &json!({"path":"f.rs","hunks":[{"old_line_start":4,"old_line_count":1,"new_line_start":4,"new_line_count":2},
                                           {"old_line_start":9,"old_line_count":0,"new_line_start":10,"new_line_count":1}]}),
        );
        assert_eq!(strs(&diff), vec!["-f.rs:4", "+f.rs:4", "+f.rs:5", "+f.rs:10"]);
        let structural = extract_items(
            EvalMode::DiffStructural,
            &Value::Null,
            &json!({"added":[{"name":"a","kind":"function"}],"removed":["b"]}),
        );
        assert_eq!(strs(&structural), vec!["+a", "-b"]);
        let impls = extract_items(
            EvalMode::Implementations,
            &Value::Null,
            &json!({"hits":[{"path":"x.rs","line":3,"impl_type":"T"}]}),
        );
        assert_eq!(strs(&impls), vec!["x.rs:3"]);
    }
}
