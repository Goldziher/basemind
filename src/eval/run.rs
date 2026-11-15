//! Execute one task against a one-shot in-process server (the same tool bodies the MCP surface
//! runs), score it, and run its token-savings baseline.

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
use super::extract::extract_items;
use super::report::{SavingsOutcome, TaskResult};
use super::score::{Item, score};
use super::task::{EvalMode, Scoring, Task};

/// Call the tool behind `task.mode` with the task's `args`, which are that tool's own arguments.
/// Returns the raw response text, or a tool/argument error message.
async fn call(server: &BasemindServer, task: &Task) -> Result<String> {
    let bad_args = |message: String| anyhow!("invalid args for mode `{}`: {message}", task.mode.as_str());
    let args = if task.args.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        task.args.clone()
    };
    let result: CallToolResult = match task.mode {
        EvalMode::GitSearch => server.git_log_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Docs => server.docs_search_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Symbols => server.code_symbols_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Outline => server.code_outline_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::References | EvalMode::Callers | EvalMode::Dependents => {
            server.code_refs_call(parse_args(args).map_err(bad_args)?).await
        }
        EvalMode::Grep => server.code_grep_call(parse_args(args).map_err(bad_args)?).await,
        EvalMode::Find => server.find_files_call(parse_args(args).map_err(bad_args)?).await,
    }
    .map_err(|e| anyhow!("{}: {e}", task.mode.tool_key()))?;

    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if result.is_error == Some(true) {
        return Err(anyhow!("{}: {}", task.mode.tool_key(), text));
    }
    Ok(text)
}

/// Run `task`, scoring the answer and (when the task has a baseline) measuring token savings.
pub async fn run_task(server: &BasemindServer, root: &Path, task: &Task, min_recall: f64) -> TaskResult {
    let started = Instant::now();
    let outcome = call(server, task).await;
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);

    let (text, error) = match outcome {
        Ok(t) => (t, None),
        Err(e) => (String::new(), Some(format!("{e:#}"))),
    };
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let returned: Vec<Item> = extract_items(task.mode, &task.args, &body);
    let gold: Vec<Item> = task.gold.iter().map(|g| Item::parse(g)).collect();
    let ranked = task.scoring == Scoring::Ranked;
    let sc = score(&returned, &gold, task.line_slack, ranked.then(|| task.k()));
    let tokens = count_tokens(&text);

    let (savings, baseline_error) = match (&task.baseline, &error) {
        (Some(spec), None) => match run_baseline(root, spec, count_tokens) {
            Ok(b) => {
                let model = estimate_from_text(task.mode.tool_key(), 0, &text);
                let has_answer = tokens > 0;
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
                    None,
                )
            }
            Err(e) => (None, Some(format!("{e:#}"))),
        },
        _ => (None, None),
    };

    TaskResult {
        id: task.id.clone(),
        mode: task.mode,
        ok: error.is_none(),
        error,
        elapsed_us,
        tokens,
        score: sc,
        ranked,
        returned: returned
            .iter()
            .take(20)
            .map(|i| match i.line {
                Some(l) => format!("{}:{l}", i.path),
                None => i.path.clone(),
            })
            .collect(),
        savings,
        baseline_error,
    }
}
