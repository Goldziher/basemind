//! `code_refs` relation `definition` in a daemon-hosted session, which reads the pool writer's
//! index while its own file map can trail it.

use std::sync::Arc;

use crate::comms::workspace_pool::WorkspacePool;
use crate::engine::Workspace;
use crate::engine::definition::SNAPSHOT_REFUSED;
use crate::engine::types::refs::CodeRefsParams;
use crate::path::RelPath;

const A_SRC: &str = "export function helper() {\n  return 1;\n}\n";
const B_SRC: &str = "import { helper } from './a';\nexport function run() {\n  return helper();\n}\n";

/// `code_refs` relation `definition` for the use of `helper` in `b.ts`.
fn goto(ws: &Workspace) -> serde_json::Value {
    let params = CodeRefsParams {
        name: Some("helper".to_string()),
        relation: Some("definition".to_string()),
        path: Some(RelPath::from("b.ts")),
        line: Some(3),
        column: None,
        depth: None,
        match_mode: None,
        limit: None,
        cursor: None,
        max_tokens: None,
        resolution: None,
        changed: None,
    };
    serde_json::to_value(ws.code_refs(params).expect("goto")).expect("the answer serializes")
}

/// While the writer's resolve pass is unsettled (running, cancelled after the flush, or aborted),
/// its cross-file edges can belong to the files' previous contents. A hosted session reads that
/// index directly, so the hop is refused and says the index is updating instead of naming a line of
/// other content; the use's own import binding is still served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_cross_file_definition_is_refused_while_the_resolve_pass_is_unsettled() {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git init");
    assert!(status.success());
    std::fs::write(root.join("a.ts"), A_SRC).expect("a.ts");
    std::fs::write(root.join("b.ts"), B_SRC).expect("b.ts");

    let pool = Arc::new(WorkspacePool::new(2));
    pool.rescan(root, None, true, false, &crate::scanner::ScanCancel::default())
        .expect("scan");
    let served = pool
        .get_or_build_serve_state(root, |_| None)
        .await
        .expect("hosted stack");
    if served.workspace.store_read().lookup("a.ts").is_none() {
        crate::testing::unavailable(
            "BASEMIND_TEST_ALLOW_NO_GRAMMAR",
            "the typescript grammar is unavailable",
        );
        return;
    }
    let settled = goto(&served.workspace);
    assert_eq!(
        settled["definition"]["path"], "a.ts",
        "a settled pass serves the cross-file definition: {settled}"
    );

    served
        .workspace
        .store_read()
        .mark_status_health(&crate::store::ScanHealth::resolving());
    let unsettled = goto(&served.workspace);
    assert_eq!(
        unsettled["definition"]["path"], "b.ts",
        "only the import binding is served: {unsettled}"
    );
    assert_eq!(unsettled["resolution_note"], SNAPSHOT_REFUSED, "{unsettled}");
}
