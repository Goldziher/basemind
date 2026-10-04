//! Execute one task against a one-shot in-process server (the same `#[tool]` methods the MCP
//! surface dispatches), score it, and run its token-savings baseline.

use std::path::Path;
use std::time::Instant;

use anyhow::{Result, anyhow};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value};

use crate::mcp::BasemindServer;
use crate::mcp::mode::{CodeMode, GitMode, MemoryMode};
use crate::mcp::params::{CodeParams, GitParams, Lenient, Parameters};
use crate::mcp::savings::estimate_from_text;
use crate::mcp::tokens::count_tokens;
use crate::mcp::types_memory::MemoryParams;

use super::baseline::run_baseline;
use super::extract::extract_items;
use super::report::{SavingsOutcome, TaskResult};
use super::score::{Item, score};
use super::task::{EvalMode, Scoring, Task};

/// Tool arguments with the `mode` discriminator the tool's wire format wants.
fn wire_args(mode: &str, args: &Value) -> Map<String, Value> {
    let mut m = args.as_object().cloned().unwrap_or_default();
    m.insert("mode".into(), Value::String(mode.into()));
    m
}

/// Call the tool behind `mode`. Returns the raw response text, or a tool/argument error message.
async fn call(server: &BasemindServer, task: &Task) -> Result<String> {
    let bad_args = |e: serde_json::Error| anyhow!("invalid args for mode `{}`: {e}", task.mode.as_str());
    let result: CallToolResult = match task.mode {
        EvalMode::GitSearch => {
            let p: GitParams = serde_json::from_value(Value::Object(wire_args(GitMode::Search.as_str(), &task.args)))
                .map_err(bad_args)?;
            server.git(Parameters(Lenient(p))).await
        }
        EvalMode::Docs => {
            let p: MemoryParams =
                serde_json::from_value(Value::Object(wire_args(MemoryMode::Documents.as_str(), &task.args)))
                    .map_err(bad_args)?;
            server.memory(Parameters(Lenient(p))).await
        }
        other => {
            let code_mode = match other {
                EvalMode::Symbols => CodeMode::Symbols,
                EvalMode::Outline => CodeMode::Outline,
                EvalMode::References => CodeMode::References,
                EvalMode::Callers => CodeMode::Callers,
                EvalMode::Grep => CodeMode::Grep,
                EvalMode::Find => CodeMode::Find,
                EvalMode::Dependents => CodeMode::Dependents,
                EvalMode::GitSearch | EvalMode::Docs => unreachable!("handled above"),
            };
            let p: CodeParams =
                serde_json::from_value(Value::Object(wire_args(code_mode.as_str(), &task.args))).map_err(bad_args)?;
            server.code(Parameters(Lenient(p))).await
        }
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
