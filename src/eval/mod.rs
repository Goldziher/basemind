//! `basemind admin eval` — retrieval-quality and token-savings evaluation.
//!
//! Reads a JSONL task file (see [`task`]), runs every task through the same tool code the MCP
//! server dispatches (in-process, one-shot server over the existing index), and scores the
//! answer against gold ([`score`]). Tasks that carry a `baseline` additionally run the
//! grep/read-style alternative in the workspace root; its output is counted with the same
//! tokenizer, and the savings count only when the basemind answer was correct enough
//! (`--min-recall`). The per-mode aggregate ([`report`]) is written as JSON and markdown and can
//! be compared to an earlier report to gate regressions.
//!
//! Gold generators that are repo-agnostic live under `benchmarks/eval/`.

mod baseline;
mod extract;
mod report;
mod run;
mod score;
mod task;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::mcp::BasemindServer;
use crate::mcp::tokens::TOKENS_ARE_COUNTED;

pub use report::{Regression, Report, TaskResult, aggregate, compare, markdown};
pub use score::{Item, Score, score};
pub use task::{EvalMode, Scoring, Task, parse_tasks};

/// Arguments of `basemind admin eval`.
#[derive(Args, Debug, Clone)]
pub struct EvalArgs {
    /// JSONL task file: one `{id, mode, args, gold, scoring, k, baseline}` object per line.
    #[arg(long, value_name = "FILE")]
    pub tasks: PathBuf,
    /// Write the per-task NDJSON results here (default: stdout).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Write the aggregate per-mode report as JSON here.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// Write the aggregate report as markdown here.
    #[arg(long, value_name = "FILE")]
    pub markdown: Option<PathBuf>,
    /// Compare against an earlier `--report` file; exit non-zero on regression.
    #[arg(long, value_name = "REPORT_JSON")]
    pub baseline: Option<PathBuf>,
    /// Allowed absolute drop in a quality metric (F1, recall, hit@n, MRR, nDCG) before it counts
    /// as a regression.
    #[arg(long, default_value_t = 0.02)]
    pub tolerance: f64,
    /// Allowed relative increase in p95 latency and mean response tokens.
    #[arg(long, default_value_t = 0.25)]
    pub cost_tolerance: f64,
    /// Recall a task must reach before its token savings are credited.
    #[arg(long, default_value_t = 0.8)]
    pub min_recall: f64,
    /// Run every task once untimed first, so the measured run sees a warm server.
    #[arg(long)]
    pub warmup: bool,
    /// Run only tasks of these modes (repeatable).
    #[arg(long = "mode", value_name = "MODE", value_parser = parse_mode)]
    pub modes: Vec<EvalMode>,
}

fn parse_mode(s: &str) -> std::result::Result<EvalMode, String> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(|_| {
        format!("unknown mode `{s}` (symbols, outline, references, callers, grep, find, dependents, git_search, docs)")
    })
}

/// Run the evaluation. Returns an error (non-zero exit) on a bad task file or a regression.
pub async fn run(server: &BasemindServer, args: &EvalArgs, out: &mut impl Write) -> Result<()> {
    let text = std::fs::read_to_string(&args.tasks).with_context(|| format!("read {}", args.tasks.display()))?;
    let mut tasks = parse_tasks(&text)?;
    if !args.modes.is_empty() {
        tasks.retain(|t| args.modes.contains(&t.mode));
    }
    if tasks.is_empty() {
        bail!("no tasks to run in {}", args.tasks.display());
    }
    let root = server.state.shared.root.clone();

    if args.warmup {
        for t in &tasks {
            let _ = run::run_task(server, &root, t, args.min_recall).await;
        }
    }
    let mut results = Vec::with_capacity(tasks.len());
    for t in &tasks {
        results.push(run::run_task(server, &root, t, args.min_recall).await);
    }

    let ndjson: String = results
        .iter()
        .map(|r| serde_json::to_string(r).map(|l| l + "\n"))
        .collect::<Result<_, _>>()
        .context("serialize results")?;
    match &args.out {
        Some(p) => std::fs::write(p, &ndjson).with_context(|| format!("write {}", p.display()))?,
        None => out.write_all(ndjson.as_bytes()).context("write results")?,
    }

    let tokenizer = if TOKENS_ARE_COUNTED { "o200k" } else { "bytes/4" };
    let rep = aggregate(
        &results,
        tokenizer,
        &root.display().to_string(),
        &args.tasks.display().to_string(),
        args.min_recall,
    );
    if let Some(p) = &args.report {
        let json = serde_json::to_string_pretty(&rep).context("serialize report")?;
        std::fs::write(p, json + "\n").with_context(|| format!("write {}", p.display()))?;
    }
    let md = markdown(&rep);
    match &args.markdown {
        Some(p) => std::fs::write(p, &md).with_context(|| format!("write {}", p.display()))?,
        None => eprintln!("{md}"),
    }
    for r in results.iter().filter(|r| !r.ok) {
        eprintln!("task {} failed: {}", r.id, r.error.as_deref().unwrap_or("?"));
    }

    if let Some(path) = &args.baseline {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let base: Report = serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if base.tokenizer != rep.tokenizer {
            bail!(
                "baseline report used tokenizer `{}` but this run used `{}`; token counts are not comparable",
                base.tokenizer,
                rep.tokenizer
            );
        }
        let regressions = compare(&base, &rep, args.tolerance, args.cost_tolerance);
        if !regressions.is_empty() {
            for r in &regressions {
                eprintln!("REGRESSION {r}");
            }
            bail!("{} regression(s) against {}", regressions.len(), path.display());
        }
        eprintln!("no regressions against {}", path.display());
    }
    Ok(())
}
