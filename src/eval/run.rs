//! Execute one task against a one-shot in-process server (the same tool bodies the MCP surface
//! runs), score it, and run and score its shell baseline.

use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use crate::mcp::BasemindServer;
use crate::mcp::params::parse_args;
use crate::mcp::savings::estimate_from_text;
use crate::mcp::tokens::count_tokens;

use super::baseline::run_baseline;
use super::extract::{annotation_accuracy, extract_items, grep_annotations, grep_ranges};
use super::report::{SavingsOutcome, TaskResult};
use super::score::{Item, score};
use super::task::{EvalMode, Scoring, Task};

/// Most pages a set-scored task follows `next_cursor` through. An agent that wants every call
/// site or every commit has to page (ADR-0022 contract rule 5), and pays for every page.
pub const MAX_PAGES: u32 = 50;

/// Why every task of `mode` is skipped: the engine op or tool argument it drives has not landed
/// (ADR-0047). A skipped task is reported, never run, scored or failed; the change that lands the op
/// removes its arm here.
pub fn unavailable(mode: EvalMode) -> Option<&'static str> {
    match mode {
        EvalMode::Impact => Some("not implemented: `code_refs` `importers` with `path` and `depth` (#121)"),
        EvalMode::Cycles => Some("not implemented: `code_refs` relation `cycles` (#135)"),
        EvalMode::GrepChanged => Some("not implemented: `code_grep` `changed` (#122)"),
        EvalMode::FindLarge => Some("not implemented: `find_files` `min_lines` (#127)"),
        EvalMode::ReadAround => Some("not implemented: `ranges` on `code_grep` and `code_read` (#129)"),
        EvalMode::Explain => Some("not implemented: the `code explain` engine op (#120)"),
        _ => None,
    }
}

/// Call the tool behind `mode` with `args`, which are that tool's own arguments. Returns the raw
/// response text, or a tool/argument error message. `read_around` names its second call, the
/// `code_read` of the ranges; its first is a `grep` call.
async fn call(server: &BasemindServer, mode: EvalMode, args: Value) -> Result<String> {
    let bad_args = |message: String| anyhow!("invalid args for mode `{}`: {message}", mode.as_str());
    let result: CallToolResult = match mode {
        EvalMode::GitSearch | EvalMode::GitPath | EvalMode::GitSymbol => {
            server.git_log_call(parse_args(args).map_err(bad_args)?).await
        }
        EvalMode::Blame | EvalMode::BlameSymbol => server.git_blame_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Diff | EvalMode::DiffStructural => server.git_diff_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Docs => server.docs_search_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Symbols => server.code_symbols_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Outline => server.code_outline_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Read | EvalMode::ReadAround => server.code_read_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::References
        | EvalMode::Callers
        | EvalMode::Dependents
        | EvalMode::Implementations
        | EvalMode::Impact
        | EvalMode::Cycles => server.code_refs_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Grep | EvalMode::GrepChanged => server.code_grep_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Find | EvalMode::FindLarge => server.find_files_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Explain => return Err(anyhow!("`code explain` has no engine op to call")),
    }
    .map_err(|e| anyhow!("{}: {e}", mode.tool_key()))?;

    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if result.is_error == Some(true) {
        return Err(anyhow!("{}: {}", mode.tool_key(), text));
    }
    Ok(text)
}

/// The tool's whole answer to a task: every page's items, text and tokens.
#[derive(Debug, Default)]
struct Answer {
    items: Vec<Item>,
    /// `code_grep` only: each hit's `path:line` and attached symbol name.
    annotations: Vec<(String, Option<String>)>,
    text: String,
    tokens: u64,
    pages: u32,
    /// A `next_cursor` was still offered when paging stopped at [`MAX_PAGES`].
    cut: bool,
}

impl Answer {
    /// Charge one response to the answer and return it parsed.
    fn page(&mut self, text: &str) -> Value {
        self.pages += 1;
        self.tokens += count_tokens(text);
        self.text.push_str(text);
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
}

fn task_args(task: &Task) -> Value {
    if task.args.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        task.args.clone()
    }
}

/// Call the tool, following `next_cursor` for set-scored tasks (a ranked task reads one page: the
/// agent wants the top hits, not all of them).
async fn answer(server: &BasemindServer, task: &Task) -> Result<Answer> {
    if task.mode == EvalMode::ReadAround {
        return answer_read_around(server, task).await;
    }
    let mut args = task_args(task);
    let follow = task.scoring == Scoring::Set;
    let mut out = Answer::default();
    loop {
        let text = call(server, task.mode, args.clone()).await?;
        let body = out.page(&text);
        out.items.extend(extract_items(task.mode, &task.args, &body));
        if task.mode == EvalMode::Grep {
            out.annotations.extend(grep_annotations(&body));
        }
        let next = body.get("next_cursor").and_then(Value::as_str).map(str::to_string);
        match next {
            Some(cursor) if follow && out.pages < MAX_PAGES => {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("cursor".into(), Value::String(cursor));
                }
            }
            Some(_) => {
                out.cut = follow;
                break;
            }
            None => break,
        }
    }
    Ok(out)
}

/// `read_around`: every page of `code_grep` with the task's arguments (`ranges: true`), then one
/// `code_read` per file with that file's ranges. Every call is charged; the items are the lines read.
async fn answer_read_around(server: &BasemindServer, task: &Task) -> Result<Answer> {
    let mut args = task_args(task);
    let mut out = Answer::default();
    let mut reads = Vec::new();
    loop {
        let text = call(server, EvalMode::Grep, args.clone()).await?;
        let body = out.page(&text);
        reads.extend(grep_ranges(&body));
        match body.get("next_cursor").and_then(Value::as_str) {
            Some(cursor) if out.pages < MAX_PAGES => {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert("cursor".into(), Value::String(cursor.to_string()));
                }
            }
            Some(_) => {
                out.cut = true;
                break;
            }
            None => break,
        }
    }
    for (path, ranges) in reads {
        let read = serde_json::json!({ "path": path, "ranges": ranges });
        let text = call(server, EvalMode::ReadAround, read.clone()).await?;
        let body = out.page(&text);
        out.items.extend(extract_items(EvalMode::ReadAround, &read, &body));
    }
    Ok(out)
}

/// The row of a task [`unavailable`] skips: not run, not scored, not an error.
fn skipped(task: &Task, reason: &str) -> TaskResult {
    TaskResult {
        id: task.id.clone(),
        mode: task.mode,
        ok: true,
        error: None,
        skipped: Some(reason.to_string()),
        elapsed_us: 0,
        tokens: 0,
        pages: 0,
        pages_cut: false,
        annotation_accuracy: None,
        score: super::score::Score::default(),
        ranked: task.scoring == Scoring::Ranked,
        returned: Vec::new(),
        savings: None,
        baseline_score: None,
        baseline_error: None,
    }
}

/// Run `task`, scoring the answer and (when the task has a baseline) running and scoring that too.
pub async fn run_task(server: &BasemindServer, root: &Path, task: &Task, min_recall: f64) -> TaskResult {
    if let Some(reason) = unavailable(task.mode) {
        return skipped(task, reason);
    }
    let started = Instant::now();
    let outcome = answer(server, task).await;
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);

    let (ans, error) = match outcome {
        Ok(a) => (a, None),
        Err(e) => (Answer::default(), Some(format!("{e:#}"))),
    };
    let gold: Vec<Item> = task.gold.iter().map(|g| Item::parse(g)).collect();
    let ranked = task.scoring == Scoring::Ranked;
    let k = ranked.then(|| task.k());
    let sc = score(&ans.items, &gold, task.line_slack, k);
    let tokens = ans.tokens;
    let annotations = annotation_accuracy(&ans.annotations, &task.annotations);

    let (savings, baseline_score, baseline_error) = match (&task.baseline, &error) {
        (Some(spec), None) => match run_baseline(root, spec, count_tokens) {
            Ok(b) => {
                let model = estimate_from_text(task.mode.tool_key(), 0, &ans.text);
                let has_answer = tokens > 0;
                let baseline_score = b.items.as_ref().map(|items| score(items, &gold, task.line_slack, k));
                (
                    Some(SavingsOutcome {
                        baseline_tokens: b.tokens,
                        baseline_elapsed_us: b.elapsed_us,
                        ratio: has_answer.then(|| b.tokens as f64 / tokens as f64),
                        saved_tokens: b.tokens as i64 - tokens as i64,
                        credited: sc.recall >= min_recall && !gold.is_empty(),
                        model_ratio: has_answer.then(|| model.baseline_tokens as f64 / model.actual_tokens as f64),
                        model_label: model.baseline.to_string(),
                    }),
                    baseline_score,
                    None,
                )
            }
            Err(e) => (None, None, Some(format!("{e:#}"))),
        },
        _ => (None, None, None),
    };

    TaskResult {
        id: task.id.clone(),
        mode: task.mode,
        ok: error.is_none(),
        error,
        skipped: None,
        elapsed_us,
        tokens,
        pages: ans.pages,
        pages_cut: ans.cut,
        annotation_accuracy: annotations,
        score: sc,
        ranked,
        returned: ans
            .items
            .iter()
            .take(20)
            .map(|i| match i.line {
                Some(l) => format!("{}:{l}", i.path),
                None => i.path.clone(),
            })
            .collect(),
        savings,
        baseline_score,
        baseline_error,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::eval::task::parse_tasks;

    const ANALYSIS: [EvalMode; 6] = [
        EvalMode::Impact,
        EvalMode::Cycles,
        EvalMode::GrepChanged,
        EvalMode::FindLarge,
        EvalMode::ReadAround,
        EvalMode::Explain,
    ];

    #[test]
    fn skips_exactly_the_kinds_whose_op_has_not_landed() {
        for mode in ANALYSIS {
            let reason = unavailable(mode).unwrap_or_else(|| panic!("{mode:?} has an op"));
            assert!(reason.starts_with("not implemented"), "{reason}");
        }
        assert!(unavailable(EvalMode::Grep).is_none());
        assert!(unavailable(EvalMode::DiffStructural).is_none());
    }

    #[test]
    fn a_skipped_task_is_neither_an_error_nor_scored() {
        let task =
            &parse_tasks("{\"id\":\"i\",\"mode\":\"impact\",\"gold\":[\"a.rs\"],\"scoring\":\"set\"}").unwrap()[0];
        let r = skipped(task, "not implemented: x");
        assert!(r.ok && r.error.is_none());
        assert_eq!(r.skipped.as_deref(), Some("not implemented: x"));
        assert_eq!((r.tokens, r.pages, r.score.gold), (0, 0, 0));
        assert!(r.savings.is_none());
    }

    fn scored(mode: EvalMode, args: &Value, body: &Value, gold: &[&str]) -> (f64, f64) {
        let got = extract_items(mode, args, body);
        let gold: Vec<Item> = gold.iter().map(|g| Item::parse(g)).collect();
        let s = score(&got, &gold, 0, None);
        (s.precision, s.recall)
    }

    #[test]
    fn scores_impact_as_the_set_of_importing_files() {
        let body = json!({"hits":[{"path":"a.rs","depth":1},{"path":"b.rs","depth":2},{"path":"x.rs","depth":1}]});
        let (p, r) = scored(EvalMode::Impact, &Value::Null, &body, &["a.rs", "b.rs", "c.rs", "d.rs"]);
        assert_eq!((p, r), (2.0 / 3.0, 0.5));
    }

    #[test]
    fn scores_cycles_by_member_file_once_per_file() {
        // `a.rs` sits in two returned cycles; it counts once.
        let body = json!({"cycles":[{"files":["a.rs","b.rs"]},{"files":["a.rs","c.rs"]}]});
        let (p, r) = scored(EvalMode::Cycles, &Value::Null, &body, &["a.rs", "b.rs", "c.rs"]);
        assert_eq!((p, r), (1.0, 1.0));
        let (p, r) = scored(EvalMode::Cycles, &Value::Null, &json!({"cycles":[]}), &["a.rs"]);
        assert_eq!((p, r), (0.0, 0.0));
    }

    #[test]
    fn scores_grep_changed_at_line_level() {
        let body = json!({"hits":{"a.rs":[{"line":3},{"line":9}],"old.rs":[{"line":1}]},"scope_files":2});
        let (p, r) = scored(EvalMode::GrepChanged, &Value::Null, &body, &["a.rs:3", "a.rs:9"]);
        assert_eq!((p, r), (2.0 / 3.0, 1.0), "a hit outside the change costs precision");
    }

    #[test]
    fn scores_find_large_as_a_path_set() {
        let body = json!({"files":[{"path":"big.rs","lines":3000},{"path":"mid.rs","lines":2100}]});
        let (p, r) = scored(
            EvalMode::FindLarge,
            &Value::Null,
            &body,
            &["big.rs", "mid.rs", "huge.rs"],
        );
        assert_eq!((p, r), (1.0, 2.0 / 3.0));
    }

    #[test]
    fn scores_read_around_per_line_read() {
        let body = json!({"path":"f.rs","spans":[{"line":1,"end_line":4},{"line":10,"end_line":11}]});
        let gold = ["f.rs:1", "f.rs:2", "f.rs:3", "f.rs:4", "f.rs:5", "f.rs:10", "f.rs:11"];
        let (p, r) = scored(EvalMode::ReadAround, &Value::Null, &body, &gold);
        assert_eq!((p, r), (1.0, 6.0 / 7.0));
    }

    #[test]
    fn scores_explain_over_definition_callers_and_callees() {
        let body = json!({"definition":{"path":"a.rs","line":3},"callers":{"b.rs":[{"line":7}]},
                          "callees":["spawn","extra"]});
        let (p, r) = scored(
            EvalMode::Explain,
            &Value::Null,
            &body,
            &["a.rs:3", "b.rs:7", "c.rs:2", "->spawn"],
        );
        assert_eq!((p, r), (0.75, 0.75));
    }
}
