//! `basemind code cycles` (#135): the import cycles of the file-level graph, or with
//! `--check FROM TO` whether that import would close one; `--fail-on-cycle` makes a listed cycle
//! fail the run, for CI. Engine op: `Workspace::code_cycles`.

use clap::Args;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::engine::Workspace;
use crate::engine::types::graph::{CyclesParams, RUST_CYCLE_NOTE};

use super::super::context::CliContext;
use super::super::exit::{CliExit, ERROR};
use super::super::resolve_path;

#[derive(Args, Debug)]
pub struct CyclesArgs {
    /// Only cycles with a file at this path or under this directory.
    #[arg(long)]
    path: Option<String>,
    /// Leave out cycles of more files than this.
    #[arg(long)]
    max_size: Option<u32>,
    /// Whether FROM importing TO would close a cycle, and the chain from TO back to FROM it would
    /// close.
    #[arg(long, num_args = 2, value_names = ["FROM", "TO"], conflicts_with_all = ["path", "max_size"])]
    check: Option<Vec<String>>,
    /// Also follow type-only imports (`import type`).
    #[arg(long)]
    include_type_imports: bool,
    /// Also follow Rust `mod x;` declarations from a parent module to its child.
    #[arg(long)]
    include_module_decls: bool,
    /// Exit 1 when a cycle is listed (after `--path` and `--max-size`), for CI. A cycle among Rust
    /// modules of one crate, which the compiler accepts, does not fail the run unless `--strict` is
    /// given. A report from an index that is not ready also exits 1, saying why, instead of passing.
    #[arg(long, conflicts_with = "check")]
    pub(super) fail_on_cycle: bool,
    /// With `--fail-on-cycle`, also fail on a cycle among Rust modules of one crate.
    #[arg(long, requires = "fail_on_cycle")]
    pub(super) strict: bool,
}

pub(super) async fn call(tools: &CliContext, args: CyclesArgs) -> Result<CallToolResult, McpError> {
    let (check_from, check_to) = match args.check.as_deref() {
        Some([from, to]) => (Some(resolve_path(tools, from)), Some(resolve_path(tools, to))),
        _ => (None, None),
    };
    let p = CyclesParams {
        path: args.path.map(|path| resolve_path(tools, &path).to_string()),
        max_size: args.max_size,
        check_from,
        check_to,
        include_type_imports: args.include_type_imports,
        include_module_decls: args.include_module_decls,
    };
    tools
        .call("code_cycles", p, async |s, p| s.cli_op(p, Workspace::code_cycles).await)
        .await
}

/// `--fail-on-cycle`: exit 1 on a listed cycle other than one among Rust modules of one crate.
pub(super) fn fail_on_cycle(result: &CallToolResult) -> anyhow::Result<()> {
    gate(result, false)
}

/// `--fail-on-cycle --strict`: exit 1 on any listed cycle.
pub(super) fn fail_on_any_cycle(result: &CallToolResult) -> anyhow::Result<()> {
    gate(result, true)
}

fn gate(result: &CallToolResult, strict: bool) -> anyhow::Result<()> {
    let report = super::super::render::result_to_value(result)?;
    if report["notice"]["retry"] == true {
        return Err(CliExit::with_code(
            ERROR,
            format!(
                "cycles: the index is not ready, so the gate judged nothing: {}",
                report["notice"]["message"].as_str().unwrap_or_default()
            ),
        ));
    }
    let failing = report["cycles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|cycle| strict || cycle["note"] != RUST_CYCLE_NOTE)
        .count();
    if failing == 0 {
        return Ok(());
    }
    Err(CliExit::with_code(
        ERROR,
        format!("cycles: {failing} import cycle(s) listed"),
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::cli::exit::exit_code_for;

    fn report(cycles: Value) -> CallToolResult {
        CallToolResult::structured(json!({ "cycle_count": 1, "cycles": cycles }))
    }

    /// The exit code and message of a gate that failed.
    fn failure(outcome: anyhow::Result<()>) -> (u8, String) {
        let error = outcome.expect_err("the gate fails");
        (exit_code_for(&error), error.to_string())
    }

    #[test]
    fn a_rust_module_cycle_fails_only_the_strict_gate() {
        let rust = report(json!([{ "size": 2, "note": RUST_CYCLE_NOTE }]));
        assert!(fail_on_cycle(&rust).is_ok());
        assert_eq!(
            failure(fail_on_any_cycle(&rust)),
            (ERROR, "cycles: 1 import cycle(s) listed".to_string())
        );
    }

    #[test]
    fn any_other_cycle_fails_both_gates() {
        let cycles = report(json!([
            { "size": 2 },
            { "size": 3, "note": "go: an import cycle does not compile, so the index is stale or the build is broken" },
            { "size": 4, "note": RUST_CYCLE_NOTE },
        ]));
        assert_eq!(
            failure(fail_on_cycle(&cycles)),
            (ERROR, "cycles: 2 import cycle(s) listed".to_string())
        );
        assert_eq!(
            failure(fail_on_any_cycle(&cycles)),
            (ERROR, "cycles: 3 import cycle(s) listed".to_string())
        );
    }

    #[test]
    fn no_cycle_passes_both_gates() {
        let clean = report(json!([]));
        assert!(fail_on_cycle(&clean).is_ok());
        assert!(fail_on_any_cycle(&clean).is_ok());
    }

    #[test]
    fn a_report_from_an_index_not_ready_fails_instead_of_passing() {
        let warming = CallToolResult::structured(json!({
            "cycle_count": 0,
            "cycles": [],
            "notice": { "state": "building_index", "message": "Index is building.", "retry": true },
        }));
        assert_eq!(
            failure(fail_on_cycle(&warming)),
            (
                ERROR,
                "cycles: the index is not ready, so the gate judged nothing: Index is building.".to_string()
            )
        );
        let rescanning = CallToolResult::structured(json!({
            "cycle_count": 0,
            "cycles": [],
            "notice": { "state": "rescanning", "message": "usable", "retry": false },
        }));
        assert!(fail_on_cycle(&rescanning).is_ok());
    }
}
