//! Execute one task against a one-shot in-process server (the same tool bodies the MCP surface
//! runs), score it, and run and score its shell baseline.

use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use crate::dispatch::LocalEngine;
use crate::engine::Workspace;
use crate::engine::savings::estimate_from_text;
use crate::mcp::params::parse_args;
use crate::mcp::tokens::count_tokens;

use super::baseline::run_baseline;
use super::extract::{annotation_accuracy, extract_items, grep_annotations};
use super::report::{SavingsOutcome, TaskResult};
use super::score::{Item, score};
use super::task::{EvalMode, Scoring, Task};

/// Most pages a set-scored task follows `next_cursor` through. An agent that wants every call
/// site or every commit has to page (ADR-0022 contract rule 5), and pays for every page.
pub const MAX_PAGES: u32 = 50;

/// Call the tool behind `mode` with `args`, which are that tool's own arguments. Returns the raw
/// response text, or a tool/argument error message. `read_around` is one `code_grep` with
/// `context`, the lines around each hit written as rows.
async fn call(server: &LocalEngine, mode: EvalMode, args: Value) -> Result<String> {
    let bad_args = |message: String| anyhow!("invalid args for mode `{}`: {message}", mode.as_str());
    let result: CallToolResult = match mode {
        EvalMode::GitSearch | EvalMode::GitPath | EvalMode::GitSymbol => {
            server.op(parse_args(args).map_err(bad_args)?, Workspace::git_log).await
        }
        EvalMode::Blame | EvalMode::BlameSymbol => {
            server
                .op(parse_args(args).map_err(bad_args)?, Workspace::git_blame)
                .await
        }
        EvalMode::Diff | EvalMode::DiffStructural => {
            server
                .op_unpaged(parse_args(args).map_err(bad_args)?, Workspace::git_diff)
                .await
        }
        EvalMode::Docs => server.docs_search(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Symbols => {
            server
                .op(parse_args(args).map_err(bad_args)?, Workspace::code_symbols)
                .await
        }
        EvalMode::Outline => {
            server
                .op(parse_args(args).map_err(bad_args)?, Workspace::code_outline)
                .await
        }
        EvalMode::Read => {
            server
                .op_unpaged(parse_args(args).map_err(bad_args)?, Workspace::code_read)
                .await
        }
        EvalMode::References
        | EvalMode::Callers
        | EvalMode::Dependents
        | EvalMode::Implementations
        | EvalMode::Impact
        | EvalMode::Cycles => {
            server
                .op(parse_args(args).map_err(bad_args)?, Workspace::code_refs)
                .await
        }
        EvalMode::Grep | EvalMode::GrepChanged | EvalMode::ReadAround => {
            server
                .op(parse_args(args).map_err(bad_args)?, Workspace::code_grep)
                .await
        }
        EvalMode::Find | EvalMode::FindLarge => {
            server
                .op(parse_args(args).map_err(bad_args)?, Workspace::find_files)
                .await
        }
        EvalMode::Explain => {
            let p = serde_json::from_value(args).map_err(|e| bad_args(e.to_string()))?;
            server.cli_op(p, Workspace::code_explain).await
        }
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
    /// Charge one response to the answer and return it read back ([`crate::wire::decode`]): the
    /// items are scored from the text an agent reads, so an answer that does not read back is a task
    /// error, never an empty answer.
    fn page(&mut self, text: &str) -> Result<Value, String> {
        self.pages += 1;
        self.tokens += count_tokens(text);
        self.text.push_str(text);
        crate::wire::decode(text)
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
async fn answer(server: &LocalEngine, task: &Task) -> Result<Answer> {
    let mut args = task_args(task);
    let follow = task.scoring == Scoring::Set;
    let mut out = Answer::default();
    loop {
        let text = call(server, task.mode, args.clone()).await?;
        let body = out.page(&text).map_err(|e| {
            let head: String = text.chars().take(200).collect();
            anyhow!("{}: unreadable answer ({e}): {head}", task.mode.tool_key())
        })?;
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

/// Run `task`, scoring the answer and (when the task has a baseline) running and scoring that too.
pub(crate) async fn run_task(server: &LocalEngine, root: &Path, task: &Task, min_recall: f64) -> TaskResult {
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
    let credited = sc.recall >= min_recall && !gold.is_empty();
    let annotations = annotation_accuracy(&ans.annotations, &task.annotations);

    // The baseline runs whether or not the tool answered, so both arms are scored on the same
    // tasks. One that errors or prints nothing is a baseline failure: recall 0 where it has an
    // `items` reader, and no ratio (`TaskResult::gated_ratio`).
    let (savings, baseline_score, baseline_error, baseline_failed) = match &task.baseline {
        Some(spec) => match run_baseline(root, spec, count_tokens) {
            Ok(b) => {
                let model = estimate_from_text(task.mode.tool_key(), 0, &ans.text);
                // A tool `savings.rs` has no model for (a CLI-only op such as `code_explain`) gets no
                // model column rather than the 1x fallback, which would read as a measured claim.
                let modelled = model.baseline != "unclassified";
                let has_answer = tokens > 0;
                let baseline_score = b.items.as_ref().map(|items| score(items, &gold, task.line_slack, k));
                (
                    Some(SavingsOutcome {
                        baseline_tokens: b.tokens,
                        baseline_elapsed_us: b.elapsed_us,
                        ratio: has_answer.then(|| b.tokens as f64 / tokens as f64),
                        saved_tokens: b.tokens as i64 - tokens as i64,
                        credited,
                        model_ratio: (has_answer && modelled)
                            .then(|| model.baseline_tokens as f64 / model.actual_tokens as f64),
                        model_label: if modelled {
                            model.baseline.to_string()
                        } else {
                            String::new()
                        },
                    }),
                    baseline_score,
                    None,
                    b.empty,
                )
            }
            Err(e) => (
                None,
                spec.items.map(|_| score(&[], &gold, task.line_slack, k)),
                Some(format!("{e:#}")),
                true,
            ),
        },
        None => (None, None, None, false),
    };

    TaskResult {
        id: task.id.clone(),
        mode: task.mode,
        ok: error.is_none(),
        error,
        elapsed_us,
        tokens,
        pages: ans.pages,
        pages_cut: ans.cut,
        annotation_accuracy: annotations,
        score: sc,
        credited,
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
        baseline_failed,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn scored(mode: EvalMode, args: &Value, body: &Value, gold: &[&str]) -> (f64, f64) {
        let got = extract_items(mode, args, body);
        let gold: Vec<Item> = gold.iter().map(|g| Item::parse(g)).collect();
        let s = score(&got, &gold, 0, None);
        (s.precision, s.recall)
    }

    #[test]
    fn scores_impact_as_the_set_of_importing_files() {
        let body = json!({"total":3,"by_depth":{"1":["a.rs","x.rs"],"2":["b.rs"]}});
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
    fn scores_read_around_per_line_shown() {
        let body = json!({"hits":{"f.rs":{"main":["1- a","2: b","3- c","4- d"],"":["10- e","11: f"]}}});
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
