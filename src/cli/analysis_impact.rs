//! `basemind code impact <target>` (#121): the files that import a file, the files defining a
//! symbol, or the files a change touches, transitively by depth. Engine op: `Workspace::code_impact`.

use clap::Args;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use crate::engine::Workspace;
use crate::engine::types::graph::ImpactParams;
use crate::path::RelPath;

use super::super::context::Tools;

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
    /// Importer levels to walk, 1 to 5 (default 1).
    #[arg(long)]
    depth: Option<u8>,
    /// Also follow type-only imports (`import type`).
    #[arg(long)]
    include_type_imports: bool,
    /// Also follow Rust `mod x;` declarations from a parent module to its child.
    #[arg(long)]
    include_module_decls: bool,
}

pub(super) async fn call(tools: &Tools, args: ImpactArgs) -> Result<CallToolResult, McpError> {
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
}
