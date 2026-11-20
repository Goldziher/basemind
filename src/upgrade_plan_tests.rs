use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::extract::SCHEMA_VER;
use crate::store::{Index, STATUS_SIDECAR_SCHEMA_VER, StatusSidecar, Store, WorkspaceMarker, init_isolated_cache};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const HEADROOM: u64 = 512 * MIB;
const FREE: u64 = 40 * GIB;
const GROUP: &str = "g1";

/// A file map whose version fields read `schema` and that holds no file.
fn map_of_schema(schema: u16) -> Vec<u8> {
    rmp_serde::to_vec_named(&serde_json::json!({ "schema_ver": schema, "files": {} })).expect("encode file map")
}

fn current_map() -> Vec<u8> {
    rmp_serde::to_vec_named(&Index::empty()).expect("encode file map")
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

/// A workspace cache directory under `data` named `key`, for `root`, with the given views.
fn plant_workspace(
    data: &DataRoot,
    key: &str,
    root: &Path,
    group: Option<&str>,
    files: Option<usize>,
    views: &[(&str, Vec<u8>)],
) {
    let dir = data.workspaces_dir().join(key);
    let marker = WorkspaceMarker {
        root: root.to_path_buf(),
        updated_unix: 0,
        group: group.map(str::to_string),
    };
    write(
        &dir.join(crate::store::WORKSPACE_MARKER_FILE),
        &serde_json::to_vec(&marker).expect("encode marker"),
    );
    if let Some(file_count) = files {
        let sidecar = StatusSidecar {
            schema_ver: STATUS_SIDECAR_SCHEMA_VER,
            file_count,
            blob_count: 0,
            scanned_unix: 0,
            resolve_complete: true,
            partial: false,
            partial_reason: None,
            progress_done: None,
            progress_total: None,
        };
        write(
            &dir.join(crate::store::STATUS_SIDECAR_FILE),
            &serde_json::to_vec(&sidecar).expect("encode sidecar"),
        );
    }
    for (view, map) in views {
        write(&dir.join(VIEWS_DIR).join(view).join(INDEX_FILE), map);
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    data: DataRoot,
    app: PathBuf,
    worktree: PathBuf,
    gone: PathBuf,
}

/// A data home left by the previous release minor: two worktrees of one repository (the main one
/// fully older, the linked one current but for a newer basemind's view), a workspace whose root is
/// gone and whose marker names no group, the group index, and the blob store with an older
/// namespace, the current one, a newer one and un-namespaced blobs.
fn older_minor_home() -> Fixture {
    let home = tempfile::tempdir().expect("tempdir");
    let data = DataRoot::new(home.path());
    let app = home.path().join("repos").join("app");
    let worktree = home.path().join("repos").join("app-wt");
    let gone = home.path().join("repos").join("gone");
    std::fs::create_dir_all(&app).expect("mkdir app");
    std::fs::create_dir_all(&worktree).expect("mkdir worktree");
    let older = map_of_schema(SCHEMA_VER - 1);
    plant_workspace(
        &data,
        "a-app",
        &app,
        Some(GROUP),
        Some(10_000),
        &[("working", older.clone()), ("staged", older.clone())],
    );
    plant_workspace(
        &data,
        "b-worktree",
        &worktree,
        Some(GROUP),
        Some(500),
        &[
            ("working", current_map()),
            ("rev-abc1234", map_of_schema(SCHEMA_VER + 1)),
        ],
    );
    plant_workspace(&data, "c-gone", &gone, None, Some(4_001), &[("working", older)]);
    let cache = data.cache_dir();
    write(
        &cache
            .join(GROUPS_DIR)
            .join(GROUP)
            .join(crate::index::INDEX_DIR)
            .join("00"),
        &vec![0; (3 * MIB) as usize],
    );
    let blobs = data.global_blobs_root();
    write(
        &blobs.join(format!("v{}", SCHEMA_VER - 1)).join("aa.fm.msgpack"),
        &vec![0; (2 * MIB) as usize],
    );
    write(
        &blobs.join(format!("v{SCHEMA_VER}")).join("bb.fm.msgpack"),
        &vec![0; KIB as usize],
    );
    write(
        &blobs.join(format!("v{}", SCHEMA_VER + 1)).join("dd.fm.msgpack"),
        &vec![0; KIB as usize],
    );
    write(&blobs.join("cc.fm.msgpack"), &vec![0; KIB as usize]);
    Fixture {
        _home: home,
        data,
        app,
        worktree,
        gone,
    }
}

fn planned(fixture: &Fixture) -> UpgradePlan {
    let mut plan = plan(&fixture.data, HEADROOM).expect("plan");
    plan.free_bytes = Some(FREE);
    plan
}

#[test]
fn the_plan_of_an_older_minor_home_names_each_rebuild_and_its_bytes() {
    let fixture = older_minor_home();
    let map = map_of_schema(SCHEMA_VER - 1).len();
    let older = SCHEMA_VER - 1;
    let newer = SCHEMA_VER + 1;
    let mut out = Vec::new();
    render_human(&planned(&fixture), &mut out).expect("render");

    let expected = format!(
        "upgrade plan for schema {SCHEMA_VER} (data home {home})
group g1: index 3.0MB replaced, frees 3.0MB, needs 516.5MB
  workspace {app}: frees {two_maps}B, 10000 files, code map scan about 3s
    view staged: rebuilt (written under schema {older}), frees {map}B
    view working: rebuilt (written under schema {older}), frees {map}B
    rebuild it alone: basemind scan --root {app}
  workspace {worktree}: frees 0B
    view rev-abc1234: refused, left in place (index written by a newer basemind (schema {newer}); this binary \
         understands {SCHEMA_VER} — upgrade basemind)
    view working: kept
no known group: frees {map}B, needs 512.0MB
  workspace {gone}: frees {map}B, 4001 files, code map scan about 2s
    view working: rebuilt (written under schema {older}), frees {map}B
    its root is gone: the orphan sweep removes this cache
blobs: frees 2.0MB once past the blob grace period, needs 2.0MB in v{SCHEMA_VER}
  v{older}: 2.0MB
  un-namespaced: 1.0KB
total: frees 5.0MB, needs 518.5MB, 40.0GB free
",
        home = fixture.data.path().display(),
        app = fixture.app.display(),
        worktree = fixture.worktree.display(),
        gone = fixture.gone.display(),
        two_maps = 2 * map,
    );
    assert_eq!(String::from_utf8(out).expect("utf-8"), expected);
}

#[test]
fn the_json_plan_carries_every_verdict_and_byte_count() {
    let fixture = older_minor_home();
    let map = map_of_schema(SCHEMA_VER - 1).len() as u64;
    let older = format!("written under schema {}", SCHEMA_VER - 1);
    let refused = format!(
        "index written by a newer basemind (schema {}); this binary understands {SCHEMA_VER} — upgrade basemind",
        SCHEMA_VER + 1
    );

    let value = serde_json::to_value(planned(&fixture)).expect("serialize");

    assert_eq!(
        value,
        serde_json::json!({
            "data_home": fixture.data.path(),
            "schema": SCHEMA_VER,
            "groups": [
                {
                    "group": GROUP,
                    "index_bytes": 3 * MIB,
                    "index_replaced": true,
                    "bytes_freed": 3 * MIB + 2 * map,
                    "bytes_needed": 3 * MIB / 2 * 3 + HEADROOM,
                    "workspaces": [
                        {
                            "key": "a-app",
                            "root": fixture.app,
                            "root_exists": true,
                            "views": [
                                {"view": "staged", "action": "rebuild", "reason": older, "older_schema": SCHEMA_VER - 1, "bytes_freed": map},
                                {"view": "working", "action": "rebuild", "reason": older, "older_schema": SCHEMA_VER - 1, "bytes_freed": map},
                            ],
                            "bytes_freed": 2 * map,
                            "files": 10_000,
                            "scan_estimate_secs": 3,
                        },
                        {
                            "key": "b-worktree",
                            "root": fixture.worktree,
                            "root_exists": true,
                            "views": [
                                {"view": "rev-abc1234", "action": "refuse", "reason": refused, "older_schema": null, "bytes_freed": 0},
                                {"view": "working", "action": "keep", "reason": null, "older_schema": null, "bytes_freed": 0},
                            ],
                            "bytes_freed": 0,
                            "files": null,
                            "scan_estimate_secs": null,
                        },
                    ],
                },
                {
                    "group": null,
                    "index_bytes": 0,
                    "index_replaced": false,
                    "bytes_freed": map,
                    "bytes_needed": HEADROOM,
                    "workspaces": [
                        {
                            "key": "c-gone",
                            "root": fixture.gone,
                            "root_exists": false,
                            "views": [
                                {"view": "working", "action": "rebuild", "reason": older, "older_schema": SCHEMA_VER - 1, "bytes_freed": map},
                            ],
                            "bytes_freed": map,
                            "files": 4_001,
                            "scan_estimate_secs": 2,
                        },
                    ],
                },
            ],
            "blobs": {
                "superseded": [{"name": format!("v{}", SCHEMA_VER - 1), "bytes": 2 * MIB}],
                "unnamespaced_bytes": KIB,
                "bytes_freed": 2 * MIB + KIB,
                "bytes_needed": 2 * MIB,
            },
            "bytes_freed": 3 * MIB + 3 * map + 2 * MIB + KIB,
            "bytes_needed": 3 * MIB / 2 * 3 + HEADROOM + 2 * MIB,
            "free_bytes": FREE,
        })
    );
}

#[test]
fn a_current_home_plans_no_rebuild_and_needs_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let data = DataRoot::new(home.path());
    let root = home.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    plant_workspace(&data, "a", &root, Some(GROUP), Some(9), &[("working", current_map())]);

    let plan = plan(&data, HEADROOM).expect("plan");

    assert_eq!((plan.bytes_freed, plan.bytes_needed), (0, 0));
    let workspace = &plan.groups[0].workspaces[0];
    assert_eq!(workspace.views[0].action, ViewAction::Keep);
    assert_eq!((workspace.files, workspace.scan_estimate_secs), (None, None));
}

/// Every entry under `dir` with its length and modification time.
fn snapshot(dir: &Path, out: &mut BTreeMap<PathBuf, (bool, u64, std::time::SystemTime)>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        let meta = std::fs::symlink_metadata(&path).expect("metadata");
        out.insert(
            path.clone(),
            (meta.is_dir(), meta.len(), meta.modified().expect("mtime")),
        );
        if meta.is_dir() {
            snapshot(&path, out);
        }
    }
}

#[test]
fn planning_and_rendering_write_nothing() {
    let fixture = older_minor_home();
    let mut before = BTreeMap::new();
    snapshot(fixture.data.path(), &mut before);

    let plan = plan(&fixture.data, HEADROOM).expect("plan");
    render_human(&plan, &mut Vec::new()).expect("render");
    serde_json::to_value(&plan).expect("serialize");

    let mut after = BTreeMap::new();
    snapshot(fixture.data.path(), &mut after);
    assert!(before.len() > 10, "the snapshot walked the fixture");
    assert_eq!(after, before);
}

/// The plan's verdict for a view is what the writer open then does: it discards exactly the file
/// maps the plan says it rebuilds, and keeps the one it says it keeps.
#[test]
fn the_plan_names_the_views_the_writer_open_rebuilds() {
    init_isolated_cache();
    let flat = rmp_serde::to_vec_named(&Index {
        blob_layout: 0,
        ..Index::empty()
    })
    .expect("encode file map");
    let cases = [
        ("older schema", map_of_schema(SCHEMA_VER - 1), ViewAction::Rebuild, true),
        ("current", current_map(), ViewAction::Keep, false),
        ("flat blob layout", flat, ViewAction::Rebuild, true),
        (
            "unreadable version",
            b"not a file map".to_vec(),
            ViewAction::SetAside,
            true,
        ),
    ];
    for (case, map, action, reset) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), VIEW_WORKING).expect("open");
        let (basemind_dir, view_dir) = (store.basemind_dir.clone(), store.view_dir.clone());
        drop(store);
        std::fs::write(view_dir.join(INDEX_FILE), &map).expect("plant the file map");

        let plan = workspace_plan(&basemind_dir).expect("plan");
        let working = plan
            .views
            .iter()
            .find(|view| view.view == VIEW_WORKING)
            .expect("working view planned");
        assert_eq!(working.action, action, "{case}");

        let reopened = loop {
            match Store::open(dir.path(), VIEW_WORKING) {
                Err(error) if error.is_lock_contention() => std::thread::sleep(std::time::Duration::from_millis(25)),
                opened => break opened.expect("reopen"),
            }
        };
        assert_eq!(reopened.reset_stale_schema, reset, "{case}");
        assert_eq!(
            std::fs::read(view_dir.join(INDEX_FILE)).ok().as_deref() == Some(map.as_slice()),
            !reset,
            "{case}: the open kept the map exactly when the plan said it would"
        );
    }
}

#[test]
fn a_scan_estimate_rounds_up_to_whole_seconds() {
    assert_eq!(format_secs(0), "0s");
    assert_eq!(format_secs(59), "59s");
    assert_eq!(format_secs(125), "2m05s");
    assert_eq!(10_000_u64.div_ceil(CODE_MAP_FILES_PER_SEC), 3);
    assert_eq!(4_000_u64.div_ceil(CODE_MAP_FILES_PER_SEC), 1);
}
