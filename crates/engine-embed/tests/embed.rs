//! What an embedder does with the engine crate alone: install the process configuration, scan a
//! fixture repository, then ask for an outline and for the callers of a function.

use std::path::Path;
use std::sync::Arc;

use basemind_engine::config::Config;
use basemind_engine::config::engine::EngineConfig;
use basemind_engine::data_root::DataRoot;
use basemind_engine::engine::types::code::CodeOutlineParams;
use basemind_engine::engine::types::refs::CodeRefsParams;
use basemind_engine::engine::workspace::OpenOptions;
use basemind_engine::engine::{Engine, Workspace};
use basemind_engine::git_cache::GitCache;
use basemind_engine::path::RelPath;
use basemind_engine::retrieval_wiring;
use basemind_engine::scanner::{EmbedMode, ScanSource, scan};
use basemind_engine::store::{Store, VIEW_WORKING};

const FIXTURE: &str = "pub fn helper() {}\n\npub fn driver() {\n    helper();\n}\n";

fn fixture_repo(root: &Path) {
    std::fs::write(root.join("lib.rs"), FIXTURE).expect("write the fixture file");
}

fn scanned_workspace(root: &Path, config: Config) -> Workspace {
    let mut store = Store::open(root, VIEW_WORKING).expect("open the store");
    let report = scan(
        root,
        &mut store,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        retrieval_wiring::backend(),
    )
    .expect("scan the fixture repository");
    assert_eq!(report.stats.updated, 1, "the fixture file is indexed");
    Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(config),
        None,
        Arc::new(GitCache::open(root, 1, false).expect("open the git cache")),
        None,
        OpenOptions {
            background: false,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
        },
    )
}

#[test]
fn a_program_scans_and_queries_a_repository_through_the_engine_crate_alone() {
    let data = tempfile::tempdir().expect("data home");
    Engine::new(EngineConfig::new(DataRoot::new(data.path()))).expect("install the engine configuration");
    let repo = tempfile::tempdir().expect("fixture repository");
    fixture_repo(repo.path());

    let workspace = scanned_workspace(repo.path(), Config::with_defaults());

    let outline = workspace
        .code_outline(CodeOutlineParams {
            path: RelPath::from("lib.rs"),
            limit: None,
            cursor: None,
            max_tokens: None,
        })
        .expect("outline of the fixture file");
    let names: Vec<&str> = outline.symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    assert_eq!(names, ["helper", "driver"]);

    let callers = workspace
        .code_refs(CodeRefsParams {
            name: Some("helper".to_owned()),
            relation: None,
            path: None,
            line: None,
            column: None,
            depth: None,
            match_mode: None,
            limit: None,
            cursor: None,
            max_tokens: None,
            resolution: None,
            changed: None,
        })
        .expect("callers of helper");
    let wire = serde_json::to_value(&callers).expect("serialize the callers");
    assert!(
        wire.to_string().contains("lib.rs"),
        "the call to helper sits in lib.rs: {wire}"
    );
}
