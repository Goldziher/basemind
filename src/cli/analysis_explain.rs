//! `basemind code explain <target>` (#120): one briefing on a symbol, a file, or the definition at
//! `path:line`. Engine op: `Workspace::code_explain`.

use clap::Args;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::engine::Workspace;
use crate::engine::types::explain::ExplainParams;

use super::super::context::CliContext;

#[derive(Args, Debug)]
pub struct ExplainArgs {
    /// A symbol name (`name`, `Type::name`), an indexed file, or `path:line` for the innermost
    /// definition holding that line.
    target: String,
    /// Sections to report, comma-separated. Symbol: body, callers, callees, importers, history.
    /// File: outline, imports, importers, calls_in, calls_out, hotspot, history, owners. Default: all.
    #[arg(long, value_delimiter = ',')]
    sections: Vec<String>,
    /// Rows per list section, 1 to 200 (default 20).
    #[arg(long)]
    limit: Option<u32>,
    /// Symbol targets: caller hops, 1 to 3 (default 1); hops past the first follow only call
    /// sites proven to bind to the definition.
    #[arg(long)]
    depth: Option<u32>,
    /// Leave the symbol's body out.
    #[arg(long)]
    no_code: bool,
    /// Token budget of the whole answer (default 6000).
    #[arg(long)]
    max_tokens: Option<u32>,
}

pub(super) async fn call(tools: &CliContext, args: ExplainArgs) -> Result<CallToolResult, McpError> {
    let p = ExplainParams {
        target: normalized(tools.root(), &args.target),
        sections: (!args.sections.is_empty()).then(|| args.sections.join(",")),
        limit: args.limit,
        depth: args.depth,
        no_code: args.no_code,
        max_tokens: args.max_tokens,
    };
    tools
        .call("code_explain", p, async |s, p| {
            s.cli_op(p, Workspace::code_explain).await
        })
        .await
}

/// `target` with a file part made repository-relative (an absolute path, `./`, `..` inside the
/// repository), its `:line` suffix kept; a symbol name is left as it is.
fn normalized(root: &std::path::Path, target: &str) -> String {
    let (path, line) = match target.rsplit_once(':') {
        Some((path, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => (path, Some(line)),
        _ => (target, None),
    };
    let file = path.contains('/') || path.contains(std::path::MAIN_SEPARATOR) || root.join(path).is_file();
    match crate::path::normalize_query_path(path, root).filter(|_| file) {
        Some(path) => match line {
            Some(line) => format!("{path}:{line}"),
            None => path,
        },
        None => target.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_target_is_made_relative_and_a_symbol_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("main.rs"), "").unwrap();
        let root = dir.path();
        assert_eq!(normalized(root, "./main.rs"), "main.rs");
        assert_eq!(normalized(root, "./src/../main.rs:12"), "main.rs:12");
        assert_eq!(normalized(root, &root.join("src/a.rs").to_string_lossy()), "src/a.rs");
        assert_eq!(normalized(root, "Workspace::new"), "Workspace::new");
        assert_eq!(normalized(root, "spawn"), "spawn");
    }
}
