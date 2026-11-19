//! `basemind code cycles` (#135): the import cycles of the file-level graph, or with
//! `--check FROM TO` whether that import would close one. Engine op: `Workspace::code_cycles`.

use clap::Args;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::engine::Workspace;
use crate::engine::types::graph::CyclesParams;

use super::super::context::Tools;
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
}

pub(super) async fn call(tools: &Tools, args: CyclesArgs) -> Result<CallToolResult, McpError> {
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
