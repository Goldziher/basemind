//! Turn a tool response (parsed JSON) into the ranked list of [`Item`]s the scorer compares to
//! gold. The item shape per mode:
//!
//! | mode | item |
//! |---|---|
//! | symbols | `path:line` of each definition (1-based; `"<path>:<line>: <signature>"` rows) |
//! | outline | `args.path:line` of each symbol in the file |
//! | references, callers | `path:line` of each call site (listed under its file) |
//! | grep | `path:line` of each match (`"<line>: <text>"` rows under their file and enclosing symbol) |
//! | find, dependents | `path` |
//! | docs | `path` of each hit chunk |
//! | git_search, git_path | the commit sha, the first word of `"<sha> <summary>"` (abbreviated; the scorer matches it as a prefix) |
//! | git_symbol | the `sha` of each history entry (abbreviated, likewise) |
//! | implementations | `path:line` of each impl (listed under its file) |
//! | read | `path:line` for every line of the returned span |
//! | blame, blame_symbol | `sha:line` for every line of each `"<line>[-<end_line>] <sha>"` hunk |
//! | diff | `+path:line` per added line, `-path:line` per removed line |
//! | diff_structural | `+name` per added symbol, `-name` per removed one |
//! | impact | `path` of each importer (`by_depth`: `{"<depth>": [path, ...]}`) |
//! | cycles | `path` of each file of each cycle (`cycles[].files`) |
//! | grep_changed | `path:line` of each match (listed under its file) |
//! | find_large | `path` of each file (`files`: `{"<path>": lines}`, or strings or `{path, lines}` rows) |
//! | read_around | `path:line` for every line of every span (`spans[]`, else the top-level span) |
//! | explain | `definition` `path:line`, each caller `path:line`, `->name` per callee |
//!
//! The analysis kinds' response shapes are the contract their engine ops (ADR-0047) answer in; an op
//! that answers otherwise changes its reader here.

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

/// `path:line` of each row of a list grouped by file: `{"<path>": [{"line": n, ...}, ...]}`.
fn grouped(body: &Value, key: &str) -> Vec<Item> {
    body.get(key)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .flat_map(|(path, rows)| {
            rows.as_array().into_iter().flatten().map(move |row| Item {
                path: path.clone(),
                line: u(row, "line"),
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
        EvalMode::References | EvalMode::Callers | EvalMode::Grep => grouped(body, "hits"),
        EvalMode::Find => array(body, "files")
            .iter()
            .filter_map(|p| p.as_str())
            .map(|p| Item {
                path: p.to_string(),
                line: None,
            })
            .collect(),
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
        EvalMode::Implementations => grouped(body, "hits"),
        EvalMode::Read => {
            let path = s(body, "path").unwrap_or_default();
            span(&path, u(body, "line"), u(body, "end_line"))
        }
        EvalMode::Blame | EvalMode::BlameSymbol => array(body, "hunks")
            .iter()
            .flat_map(|h| match h.as_str() {
                Some(row) => {
                    let (lines, rest) = row.split_once(' ').unwrap_or((row, ""));
                    let sha = rest.split(' ').next().unwrap_or_default();
                    let (first, last) = lines.split_once('-').unwrap_or((lines, lines));
                    span(sha, first.parse().ok(), last.parse().ok())
                }
                None => span(&s(h, "commit_sha").unwrap_or_default(), u(h, "line"), u(h, "end_line")),
            })
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
        EvalMode::Impact => by_depth(body),
        EvalMode::Cycles => array(body, "cycles")
            .iter()
            .flat_map(|c| array(c, "files"))
            .filter_map(|f| f.as_str().map(str::to_string).or_else(|| s(f, "path")))
            .map(|path| Item { path, line: None })
            .collect(),
        EvalMode::GrepChanged => grouped(body, "hits"),
        EvalMode::FindLarge => match body.get("files") {
            Some(Value::Object(sized)) => sized
                .keys()
                .map(|path| Item {
                    path: path.clone(),
                    line: None,
                })
                .collect(),
            _ => array(body, "files")
                .iter()
                .filter_map(|f| f.as_str().map(str::to_string).or_else(|| s(f, "path")))
                .map(|path| Item { path, line: None })
                .collect(),
        },
        EvalMode::ReadAround => {
            let path = s(body, "path").unwrap_or_default();
            let spans = array(body, "spans");
            if spans.is_empty() {
                span(&path, u(body, "line"), u(body, "end_line"))
            } else {
                spans
                    .iter()
                    .flat_map(|sp| {
                        span(
                            &s(sp, "path").unwrap_or_else(|| path.clone()),
                            u(sp, "line"),
                            u(sp, "end_line"),
                        )
                    })
                    .collect()
            }
        }
        EvalMode::Explain => {
            let def = body.get("definition");
            let mut v: Vec<Item> = def
                .and_then(|d| {
                    Some(Item {
                        path: s(d, "path")?,
                        line: u(d, "line"),
                    })
                })
                .into_iter()
                .collect();
            v.extend(match body.get("callers") {
                Some(Value::Array(rows)) => located(rows, |e| u(e, "line")),
                _ => grouped(body, "callers"),
            });
            v.extend(
                array(body, "callees")
                    .iter()
                    .filter_map(|c| c.as_str().map(str::to_string).or_else(|| s(c, "name")))
                    .map(|n| Item {
                        path: format!("->{n}"),
                        line: None,
                    }),
            );
            v
        }
    }
}

/// `path` of each importer of a `code_refs` `importers` answer over the import graph, nearest
/// depth first: `{"by_depth": {"<depth>": [path, ...]}}`.
fn by_depth(body: &Value) -> Vec<Item> {
    let mut levels: Vec<(u64, &Vec<Value>)> = body
        .get("by_depth")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(depth, paths)| Some((depth.parse().ok()?, paths.as_array()?)))
        .collect();
    levels.sort_by_key(|(depth, _)| *depth);
    levels
        .into_iter()
        .flat_map(|(_, paths)| paths.iter().filter_map(Value::as_str))
        .map(|path| Item {
            path: path.to_string(),
            line: None,
        })
        .collect()
}

/// `(path, "a-b,c-d")` per file of a `code_grep` answer with `ranges`
/// (`{"hits": {"<path>": {"ranges": [[a, b], ...]}}}`), in answer order: the `code_read` calls
/// that read those ranges.
pub fn grep_ranges(body: &Value) -> Vec<(String, String)> {
    body.get("hits")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(path, entry)| {
            let spans: Vec<String> = array(entry, "ranges")
                .iter()
                .filter_map(|r| {
                    let pair = r.as_array()?;
                    let (a, b) = (pair.first()?.as_u64()?, pair.get(1)?.as_u64()?);
                    Some(format!("{a}-{b}"))
                })
                .collect();
            (!spans.is_empty()).then(|| (path.clone(), spans.join(",")))
        })
        .collect()
}

/// `code_grep` hits as `(path:line, enclosing symbol name)`, from hits listed under their file.
pub fn grep_annotations(body: &Value) -> Vec<(String, Option<String>)> {
    body.get("hits")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .flat_map(|(path, rows)| {
            rows.as_array().into_iter().flatten().filter_map(move |row| {
                let key = format!("{path}:{}", u(row, "line")?);
                Some((key, s(row, "symbol")))
            })
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

/// The sha of each commit row: the first word of `"<sha> <summary>"`, or an object's `sha`.
fn shas(entries: &[Value]) -> Vec<Item> {
    entries
        .iter()
        .filter_map(|c| match c {
            Value::String(row) => row.split(' ').next().map(str::to_string),
            c => s(c, "sha"),
        })
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

        let refs = extract_items(EvalMode::References, &Value::Null, &json!({"hits":{"x":[{"line":7}]}}));
        assert_eq!(refs[0].line, Some(7));
        let grep = extract_items(
            EvalMode::Grep,
            &Value::Null,
            &json!({"hits":{"x":[{"line":3},{"line":9}],"y":[{"line":1}]}}),
        );
        assert_eq!(
            grep,
            vec![
                Item {
                    path: "x".into(),
                    line: Some(3)
                },
                Item {
                    path: "x".into(),
                    line: Some(9)
                },
                Item {
                    path: "y".into(),
                    line: Some(1)
                }
            ]
        );
        let find = extract_items(EvalMode::Find, &Value::Null, &json!({"files":["p/q.py"]}));
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
        let body = json!({"hits":{
            "a.rs":[{"line":3,"text":"spin()","symbol":"spin"},
                    {"line":9,"text":"w","symbol":"Wrong"},
                    {"line":12,"text":"run"}],
            "b.rs":[{"line":1,"text":"x","symbol":"x"}]
        }});
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
        let hist = extract_items(
            EvalMode::GitSymbol,
            &Value::Null,
            &json!({"history":["s1 [introduced] add it","s0 fix it"]}),
        );
        assert_eq!(strs(&hist), vec!["s1", "s0"]);
        let path = extract_items(EvalMode::GitPath, &Value::Null, &json!({"commits":["s2 docs: a b"]}));
        assert_eq!(strs(&path), vec!["s2"]);
        let files = extract_items(
            EvalMode::GitPath,
            &Value::Null,
            &json!({"commits":["0123456789ab s"],
                    "files":{"0123456789ab":{"src/a.rs":"modified","b.rs":"added"}}}),
        );
        assert_eq!(strs(&files), vec!["0123456789ab"], "a `files` map adds no items");
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
        let rows = extract_items(
            EvalMode::BlameSymbol,
            &Value::Null,
            &json!({"hunks":["2-3 c1c1c1c1c1c1","9 c2c2c2c2c2c2 old/name.rs"],"commits":{}}),
        );
        assert_eq!(strs(&rows), vec!["c1c1c1c1c1c1:2", "c1c1c1c1c1c1:3", "c2c2c2c2c2c2:9"]);
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
            &json!({"hits":{"x.rs":[{"line":3,"impl_type":"T"},{"impl_type":"U"}],"y.rs":[{"line":8,"impl_type":"V"}]}}),
        );
        assert_eq!(strs(&impls), vec!["x.rs:3", "x.rs", "y.rs:8"]);
    }

    #[test]
    fn maps_the_analysis_kinds() {
        let impact = extract_items(
            EvalMode::Impact,
            &Value::Null,
            &json!({"total":3,"by_depth":{"2":["c.rs"],"1":["a.rs","b.rs"]},"in_cycle":["b.rs"]}),
        );
        assert_eq!(strs(&impact), vec!["a.rs", "b.rs", "c.rs"]);
        let none = extract_items(EvalMode::Impact, &Value::Null, &json!({"total":0,"by_depth":{}}));
        assert!(none.is_empty());

        let cycles = extract_items(
            EvalMode::Cycles,
            &Value::Null,
            &json!({"cycles":[{"files":["a.rs","b.rs"]},
                              {"files":[{"path":"c.rs"},{"path":"d.rs"}]}]}),
        );
        assert_eq!(strs(&cycles), vec!["a.rs", "b.rs", "c.rs", "d.rs"]);

        let changed = extract_items(
            EvalMode::GrepChanged,
            &Value::Null,
            &json!({"hits":{"x.rs":[{"line":4}]},"scope_files":1}),
        );
        assert_eq!(strs(&changed), vec!["x.rs:4"]);

        let large = extract_items(
            EvalMode::FindLarge,
            &Value::Null,
            &json!({"files":[{"path":"big.rs","lines":4000},"plain.rs"]}),
        );
        assert_eq!(strs(&large), vec!["big.rs", "plain.rs"]);
        let sized = extract_items(
            EvalMode::FindLarge,
            &Value::Null,
            &json!({"files":{"big.rs":4000,"mid.rs":900}}),
        );
        assert_eq!(
            strs(&sized),
            vec!["big.rs", "mid.rs"],
            "`{{path: lines}}` keeps the answer order"
        );

        let around = extract_items(
            EvalMode::ReadAround,
            &Value::Null,
            &json!({"path":"f.rs","spans":[{"line":2,"end_line":3,"body":"x"},{"line":9,"end_line":9}]}),
        );
        assert_eq!(strs(&around), vec!["f.rs:2", "f.rs:3", "f.rs:9"]);
        let single = extract_items(
            EvalMode::ReadAround,
            &Value::Null,
            &json!({"path":"f.rs","line":5,"end_line":6}),
        );
        assert_eq!(strs(&single), vec!["f.rs:5", "f.rs:6"]);

        let explain = extract_items(
            EvalMode::Explain,
            &Value::Null,
            &json!({"definition":{"path":"a.rs","line":3},"callers":{"b.rs":[{"line":7}]},
                    "callees":["spawn",{"name":"poll"}]}),
        );
        assert_eq!(strs(&explain), vec!["a.rs:3", "b.rs:7", "->spawn", "->poll"]);
        let rows = extract_items(
            EvalMode::Explain,
            &Value::Null,
            &json!({"callers":[{"path":"b.rs","line":8}]}),
        );
        assert_eq!(strs(&rows), vec!["b.rs:8"]);
    }

    #[test]
    fn reads_grep_ranges_as_code_read_range_strings() {
        let body = json!({"hits":{"a.rs":{"ranges":[[1,8],[20,31]],"hits":3},
                                  "b.rs":{"ranges":[],"hits":0},
                                  "c.rs":{"ranges":[[4,14]],"hits":1}}});
        assert_eq!(
            grep_ranges(&body),
            vec![
                ("a.rs".to_string(), "1-8,20-31".to_string()),
                ("c.rs".to_string(), "4-14".to_string())
            ]
        );
        assert!(grep_ranges(&json!({"hits":{"a.rs":[{"line":1}]}})).is_empty());
    }

    #[test]
    fn every_git_kind_scores_its_abbreviated_sha_against_the_whole_one() {
        use crate::eval::score::score;

        let whole = "0123456789abcdef0123456789abcdef01234567";
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        let gold = [Item::parse(whole), Item::parse(other)];
        let row = |sha: &str| json!({"sha": sha, "summary": "s", "author": "a", "author_time_unix": 1});
        let bodies = [
            (
                EvalMode::GitSearch,
                json!({"commits": [row("0123456789ab"), row("fedcba987654")]}),
            ),
            (
                EvalMode::GitPath,
                json!({"commits": [row("0123456789ab"), row("fedcba987654")]}),
            ),
            (
                EvalMode::GitSymbol,
                json!({"history": [row("0123456789ab"), row("fedcba987654")], "commits_inspected": 2}),
            ),
        ];
        for (mode, body) in bodies {
            let got = extract_items(mode, &Value::Null, &body);
            let s = score(&got, &gold, 0, None);
            assert_eq!((s.precision, s.recall), (1.0, 1.0), "{mode:?}");
        }
        let blame = extract_items(
            EvalMode::Blame,
            &Value::Null,
            &json!({"hunks":[{"commit_sha": whole, "short_sha": "0123456", "line": 2}]}),
        );
        let s = score(&blame, &[Item::parse(&format!("{whole}:2"))], 0, None);
        assert_eq!(s.recall, 1.0);
    }
}
