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
use super::extract::{annotation_accuracy, extract_items, grep_annotations};
use super::report::{SavingsOutcome, TaskResult};
use super::score::{Item, score};
use super::task::{EvalMode, Scoring, Task};

/// Most pages a set-scored task follows `next_cursor` through. An agent that wants every call
/// site or every commit has to page (ADR-0022 contract rule 5), and pays for every page.
pub const MAX_PAGES: u32 = 50;

/// Call the tool behind `mode` with `args`, which are that tool's own arguments. Returns the raw
/// response text, or a tool/argument error message.
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
        EvalMode::Read => server.code_read_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::References | EvalMode::Callers | EvalMode::Dependents | EvalMode::Implementations => {
            server.code_refs_call(parse_args(args).map_err(bad_args)?).await
        }
        EvalMode::Grep => server.code_grep_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Find => server.find_files_call(parse_args(args).map_err(bad_args)?).await,
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

/// Call the tool, following `next_cursor` for set-scored tasks (a ranked task reads one page: the
/// agent wants the top hits, not all of them).
async fn answer(server: &BasemindServer, task: &Task) -> Result<Answer> {
    let mut args = if task.args.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        task.args.clone()
    };
    let follow = task.scoring == Scoring::Set;
    let mut out = Answer::default();
    loop {
        let text = call(server, task.mode, args.clone()).await?;
        out.pages += 1;
        out.tokens += count_tokens(&text);
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        out.items.extend(extract_items(task.mode, &task.args, &body));
        if task.mode == EvalMode::Grep {
            out.annotations.extend(grep_annotations(&body));
        }
        out.text.push_str(&text);
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
pub async fn run_task(server: &BasemindServer, root: &Path, task: &Task, min_recall: f64) -> TaskResult {
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
