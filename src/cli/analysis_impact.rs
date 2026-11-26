//! `basemind code impact <target>` (#121): the files that import a file, the files defining a
//! symbol, or the files a change touches, transitively by depth, with a symbol's graded callers and
//! its downstream dependencies, bucketed and in the order to work through them (#209);
//! `--fail-above` fails the run when too many files are impacted. Engine op: `Workspace::code_impact`.

use clap::Args;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::engine::Workspace;
use crate::engine::types::graph::ImpactParams;
use crate::path::RelPath;

use super::super::context::CliContext;
use super::super::exit::{CliExit, ERROR};

#[derive(Args, Debug)]
pub struct ImpactArgs {
    /// A file (`path` or `path:line`) or a symbol name. A target naming an existing file, or holding
    /// a `/`, is a file; anything else is a symbol.
    #[arg(conflicts_with_all = ["symbol", "diff", "changed"])]
    target: Option<String>,
    /// The target is this symbol, whatever files exist.
    #[arg(long, conflicts_with_all = ["diff", "changed"])]
    symbol: Option<String>,
    /// The files a change touches: a revision's merge base with `HEAD` diffed against the working
    /// tree, `A..B` or `A...B`.
    #[arg(long, conflicts_with = "changed")]
    diff: Option<String>,
    /// The uncommitted changes: `--diff HEAD`.
    #[arg(long)]
    changed: bool,
    /// Importer, import and caller levels to walk, 1 to 5 (default 1).
    #[arg(long)]
    depth: Option<u8>,
    /// Also follow type-only imports (`import type`).
    #[arg(long)]
    include_type_imports: bool,
    /// Also follow Rust `mod x;` declarations from a parent module to its child.
    #[arg(long)]
    include_module_decls: bool,
    /// Exit 1 when more than this many files are impacted (targets not counted), for CI. A report
    /// from an index that is not ready also exits 1, saying why, instead of passing.
    #[arg(long, value_name = "N")]
    pub(super) fail_above: Option<usize>,
}

pub(super) async fn call(tools: &CliContext, args: ImpactArgs) -> Result<CallToolResult, McpError> {
    let mut p = ImpactParams {
        symbol: args.symbol,
        diff: if args.changed {
            Some("HEAD".to_string())
        } else {
            args.diff
        },
        depth: args.depth,
        include_type_imports: args.include_type_imports,
        include_module_decls: args.include_module_decls,
        ..ImpactParams::default()
    };
    if let Some(target) = args.target {
        match file_target(tools.root(), &target) {
            Some(path) => p.path = Some(path),
            None => p.symbol = Some(target),
        }
    }
    tools
        .call("code_impact", p, async |s, p| s.cli_op(p, Workspace::code_impact).await)
        .await
}

/// `--fail-above <limit>`: exit 1 when the report counts more impacted files than `limit`.
pub(super) fn fail_above(limit: usize) -> super::Gate {
    Box::new(move |result| gate(result, limit))
}

fn gate(result: &CallToolResult, limit: usize) -> anyhow::Result<()> {
    let report = super::super::render::result_to_value(result)?;
    if report["notice"]["retry"] == true {
        return Err(CliExit::with_code(
            ERROR,
            format!(
                "impact: the index is not ready, so the gate judged nothing: {}",
                report["notice"]["message"].as_str().unwrap_or_default()
            ),
        ));
    }
    let impacted = report["impacted_files"]
        .as_u64()
        .ok_or_else(|| CliExit::with_code(ERROR, "impact: the report has no `impacted_files` count".to_string()))?;
    if impacted <= limit as u64 {
        return Ok(());
    }
    Err(CliExit::with_code(
        ERROR,
        format!("impact: {impacted} impacted file(s) exceed --fail-above {limit}"),
    ))
}

/// The repository file `target` names, a `:line` suffix dropped: one that exists, or any target
/// holding a `/` (the op then says whether it is indexed).
fn file_target(root: &std::path::Path, target: &str) -> Option<RelPath> {
    let path = match target.rsplit_once(':') {
        Some((path, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => path,
        _ => target,
    };
    let normalized = crate::path::normalize_query_path(path, root)?;
    (path.contains('/') || root.join(&normalized).is_file()).then(|| RelPath::from(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_a_file_when_one_exists_or_it_holds_a_slash() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("main.rs"), "").unwrap();
        let root = dir.path();
        assert_eq!(file_target(root, "main.rs"), Some(RelPath::from("main.rs")));
        assert_eq!(file_target(root, "./main.rs:12"), Some(RelPath::from("main.rs")));
        assert_eq!(file_target(root, "src/gone.rs"), Some(RelPath::from("src/gone.rs")));
        assert_eq!(file_target(root, "Workspace"), None);
        assert_eq!(file_target(root, "Workspace::new"), None);
    }

    fn report(impacted: serde_json::Value) -> CallToolResult {
        CallToolResult::structured(serde_json::json!({ "impacted_files": impacted }))
    }

    fn failure(outcome: anyhow::Result<()>) -> (u8, String) {
        let error = outcome.expect_err("the gate fails");
        (crate::cli::exit::exit_code_for(&error), error.to_string())
    }

    #[test]
    fn the_gate_passes_at_the_limit_and_fails_above_it() {
        assert!(fail_above(3)(&report(3.into())).is_ok());
        assert!(fail_above(0)(&report(0.into())).is_ok());
        assert_eq!(
            failure(fail_above(3)(&report(4.into()))),
            (ERROR, "impact: 4 impacted file(s) exceed --fail-above 3".to_string())
        );
        assert_eq!(
            failure(fail_above(0)(&report(1.into()))),
            (ERROR, "impact: 1 impacted file(s) exceed --fail-above 0".to_string())
        );
    }

    #[test]
    fn the_gate_refuses_to_judge_a_report_that_is_not_ready_or_has_no_count() {
        let not_ready = CallToolResult::structured(serde_json::json!({
            "notice": { "retry": true, "message": "scanning" }
        }));
        assert_eq!(
            failure(fail_above(10)(&not_ready)),
            (
                ERROR,
                "impact: the index is not ready, so the gate judged nothing: scanning".to_string()
            )
        );
        assert_eq!(
            failure(fail_above(10)(&report(serde_json::Value::Null))),
            (ERROR, "impact: the report has no `impacted_files` count".to_string())
        );
    }
}
