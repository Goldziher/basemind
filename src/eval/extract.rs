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
//! | git_search | the commit `sha` |

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
        // `start_row` is 0-based; gold lines are 1-based like every editor and `git grep -n`.
        EvalMode::Symbols => located(array(body, "results"), |e| u(e, "start_row").map(|r| r + 1)),
        EvalMode::Outline => {
            let path = s(body, "path").or_else(|| s(args, "path")).unwrap_or_default();
            array(body, "symbols")
                .iter()
                .map(|e| Item {
                    path: path.clone(),
                    line: u(e, "start_row").map(|r| r + 1),
                })
                .collect()
        }
        EvalMode::References | EvalMode::Callers => located(array(body, "hits"), |e| u(e, "line")),
        EvalMode::Grep => located(array(body, "hits"), |e| u(e, "line_num")),
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
        EvalMode::GitSearch => array(body, "commits")
            .iter()
            .filter_map(|c| s(c, "sha"))
            .map(|sha| Item { path: sha, line: None })
            .collect(),
    }
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
            &json!({"results":[{"path":"a.py","name":"f","start_row":0},{"path":"b.py","start_row":9}]}),
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
            &json!({"symbols":[{"name":"f","start_row":4}]}),
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
        let grep = extract_items(
            EvalMode::Grep,
            &Value::Null,
            &json!({"hits":[{"path":"x","line_num":3}]}),
        );
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
}
