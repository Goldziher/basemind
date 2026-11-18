//! BM25 in a group index (ADR-0028 rule 7): every view of a repository group shares one set of
//! postings, keyed by content and chunk settings, and each view's keyword hits and scores equal
//! those of a standalone index of the same tree.
#![cfg(feature = "code-search")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::search::bm25::bm25_search;
use basemind::store::{Store, VIEW_WORKING};

const QUERIES: [&str; 5] = [
    "process widget",
    "render frame",
    "only linked",
    "zebra quartz",
    "spawn task queue",
];

fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn source(i: usize, flavour: &str) -> String {
    format!(
        "/// Widget {i}.\npub fn process_widget_{i}() {{ render_frame(); spawn_task(); }}\n\n\
         pub fn {flavour}_helper_{i}(queue: u32) -> u32 {{ queue + {i} }}\n"
    )
}

/// A repository with two linked worktrees: the main one, one that edits two files and adds one, and
/// one that drops a file. Returns the three roots.
fn group(tmp: &Path) -> [PathBuf; 3] {
    let main = tmp.join("main");
    fs::create_dir(&main).expect("main");
    git(&["init", "-q", "-b", "main"], &main);
    git(&["config", "user.email", "t@example.com"], &main);
    git(&["config", "user.name", "Test"], &main);
    git(&["config", "core.autocrlf", "false"], &main);
    // The fixture must not depend on the user's signing setup. ~keep
    git(&["config", "commit.gpgsign", "false"], &main);
    for i in 0..12 {
        fs::write(main.join(format!("f{i}.rs")), source(i, "common")).expect("write");
    }
    git(&["add", "."], &main);
    git(&["commit", "-qm", "init"], &main);
    let linked = tmp.join("linked");
    let pruned = tmp.join("pruned");
    for (dir, branch) in [(&linked, "linked"), (&pruned, "pruned")] {
        git(
            &["worktree", "add", "-q", dir.to_str().expect("utf-8"), "-b", branch],
            &main,
        );
    }
    fs::write(linked.join("f1.rs"), source(1, "only_linked")).expect("write");
    fs::write(linked.join("f2.rs"), source(2, "zebra_quartz")).expect("write");
    fs::write(linked.join("extra.rs"), source(99, "only_linked")).expect("write");
    fs::remove_file(pruned.join("f3.rs")).expect("remove");
    [main, linked, pruned]
}

fn config(max_characters: Option<usize>) -> ConfigV1 {
    let mut config = ConfigV1::with_defaults();
    config.documents.enabled = false;
    config.code_search.enabled = true;
    config.code_search.embed = false;
    if let Some(max) = max_characters {
        config.code_search.max_characters = max;
    }
    config
}

fn scan_working(root: &Path, config: &ConfigV1) -> Store {
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    scan(
        root,
        &mut store,
        config,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    store
}

/// Copy the source files of `root` (no `.git`) to a directory outside the repository: a group of one.
fn standalone_copy(root: &Path, to: &Path) -> PathBuf {
    fs::create_dir_all(to).expect("standalone dir");
    for entry in fs::read_dir(root).expect("read worktree") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            fs::copy(&path, to.join(path.file_name().expect("name"))).expect("copy");
        }
    }
    to.to_path_buf()
}

/// One query's hits, as `(chunk id, score bits)`.
type Hits = Vec<(String, u32)>;

/// Every query's hits, plus the view's BM25 totals.
fn keyword_answers(store: &Store) -> (Option<(u64, u64)>, Vec<Hits>) {
    let view = store.index_view().expect("a writer holds its index");
    let stats = basemind::index::view::IndexView::bm25_stats(&view);
    let hits = QUERIES
        .iter()
        .map(|query| {
            bm25_search(&view, query, 50)
                .into_iter()
                .map(|hit| (hit.chunk_id, hit.score.to_bits()))
                .collect()
        })
        .collect();
    (stats, hits)
}

fn assert_equivalent(group_view: &Store, standalone: &Store, label: &str) {
    let (group_stats, group_hits) = keyword_answers(group_view);
    let (alone_stats, alone_hits) = keyword_answers(standalone);
    assert!(
        alone_hits.iter().any(|hits| !hits.is_empty()),
        "{label}: the fixture must produce keyword hits"
    );
    assert_eq!(group_stats, alone_stats, "{label}: BM25 totals");
    for ((query, group), alone) in QUERIES.iter().zip(&group_hits).zip(&alone_hits) {
        assert_eq!(group, alone, "{label}: hits and scores for {query:?}");
    }
}

#[test]
fn every_view_of_a_group_scores_like_a_standalone_index_of_its_tree() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let roots = group(tmp.path());
    // The pruned worktree cuts its chunks differently: its postings must not overwrite the others'.
    let configs = [config(None), config(None), config(Some(200))];

    let views: Vec<Store> = roots
        .iter()
        .zip(&configs)
        .map(|(root, config)| scan_working(root, config))
        .collect();
    for (i, (root, config)) in roots.iter().zip(&configs).enumerate() {
        let alone = scan_working(&standalone_copy(root, &tmp.path().join(format!("alone-{i}"))), config);
        assert_equivalent(&views[i], &alone, &format!("view {i}"));
    }

    // A full rescan of the main worktree sweeps postings: none another view still holds may go.
    drop(views);
    fs::remove_file(roots[0].join("f5.rs")).expect("remove");
    let main = scan_working(&roots[0], &configs[0]);
    drop(main);
    for (i, (root, config)) in roots.iter().zip(&configs).enumerate() {
        let view = scan_working(root, config);
        let alone = scan_working(&standalone_copy(root, &tmp.path().join(format!("again-{i}"))), config);
        assert_equivalent(&view, &alone, &format!("view {i} after a sibling's sweep"));
    }
}

/// A content another view holds keeps its postings when this view drops it; once no view holds it,
/// its postings go.
#[test]
fn postings_are_shared_by_content_and_retired_when_no_view_holds_them() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let roots = group(tmp.path());
    let config = config(None);
    let linked = scan_working(&roots[1], &config);
    drop(scan_working(&roots[0], &config));

    // f0.rs is the same content in both views; the main worktree drops it with an incremental pass.
    fs::remove_file(roots[0].join("f0.rs")).expect("remove");
    let mut main = Store::open(&roots[0], VIEW_WORKING).expect("open");
    basemind::scanner::scan_paths(
        &roots[0],
        &mut main,
        &config,
        &[roots[0].join("f0.rs")],
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("incremental rescan");
    let hits = |store: &Store| {
        let view = store.index_view().expect("index");
        bm25_search(&view, "process widget", 50)
            .into_iter()
            .map(|hit| hit.chunk_id)
            .collect::<Vec<_>>()
    };
    let f0 = linked.lookup("f0.rs").expect("indexed").hash_hex.clone();
    assert!(
        hits(&linked).iter().any(|chunk| chunk.starts_with(&f0)),
        "the linked view still finds the content the main view dropped"
    );
    assert!(
        !hits(&main).iter().any(|chunk| chunk.starts_with(&f0)),
        "the main view no longer scores it"
    );
}

/// Retirement is exact per pass (ADR-0041): after incremental passes, before any full pass sweeps,
/// every view of the group still scores like a standalone index of its tree.
#[test]
fn after_an_incremental_pass_every_view_scores_like_a_standalone_index() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let roots = group(tmp.path());
    let config = config(None);
    for root in &roots {
        drop(scan_working(root, &config));
    }

    // The linked worktree edits a file and drops another; the main one edits a file the pruned ~keep
    // worktree shares. ~keep
    fs::write(roots[1].join("f1.rs"), source(1, "render_frame_again")).expect("write");
    fs::remove_file(roots[1].join("extra.rs")).expect("remove");
    fs::write(roots[0].join("f0.rs"), source(0, "spawn_task_queue")).expect("write");
    let incremental = |root: &Path, changed: &[&str]| {
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        let paths: Vec<PathBuf> = changed.iter().map(|rel| root.join(rel)).collect();
        basemind::scanner::scan_paths(
            root,
            &mut store,
            &config,
            &paths,
            EmbedMode::Inline,
            basemind::retrieval_wiring::backend(),
        )
        .expect("incremental pass");
        store
    };
    let linked = incremental(&roots[1], &["f1.rs", "extra.rs"]);
    let main = incremental(&roots[0], &["f0.rs"]);
    let pruned = Store::open(&roots[2], VIEW_WORKING).expect("open");

    for (i, view) in [main, linked, pruned].iter().enumerate() {
        let alone = scan_working(
            &standalone_copy(&roots[i], &tmp.path().join(format!("incremental-{i}"))),
            &config,
        );
        assert_equivalent(view, &alone, &format!("view {i} after incremental passes"));
    }
}

/// Replace the source files of `to` with those of `from`.
fn mirror_sources(from: &Path, to: &Path) {
    for entry in fs::read_dir(to).expect("read worktree") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            fs::remove_file(&path).expect("remove");
        }
    }
    standalone_copy(from, to);
}

/// A new repository group whose worktrees hold the source files of `roots`, the main one first.
fn mirror_group(roots: &[&Path], at: &Path) -> Vec<PathBuf> {
    let main = at.join("main");
    fs::create_dir_all(&main).expect("main");
    git(&["init", "-q", "-b", "main"], &main);
    git(&["config", "user.email", "t@example.com"], &main);
    git(&["config", "user.name", "Test"], &main);
    git(&["config", "commit.gpgsign", "false"], &main);
    standalone_copy(roots[0], &main);
    git(&["add", "."], &main);
    git(&["commit", "-qm", "init"], &main);
    let mut mirrored = vec![main.clone()];
    for (i, root) in roots.iter().enumerate().skip(1) {
        let dir = at.join(format!("worktree-{i}"));
        let branch = format!("b{i}");
        git(
            &["worktree", "add", "-q", dir.to_str().expect("utf-8"), "-b", &branch],
            &main,
        );
        mirror_sources(root, &dir);
        mirrored.push(dir);
    }
    mirrored
}

fn group_rows(store: &Store) -> basemind::index::GroupRows {
    store
        .index_db
        .as_ref()
        .expect("a writer holds its index")
        .group_rows()
        .expect("read the group's rows")
}

/// The group sweep (ADR-0028 rule 10): after edits, an incremental pass and a worktree removed, a full
/// pass leaves the group exactly the rows a group built from scratch for the remaining worktrees
/// holds: symbols, calls, postings and blooms of the contents they name, and rows of no other view.
#[test]
fn after_a_full_pass_the_group_holds_exactly_the_rows_of_its_live_views() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let roots = group(tmp.path());
    let config = config(None);
    for root in &roots {
        drop(scan_working(root, &config));
    }

    // Main's f2 is held by main and pruned only: dead once pruned goes. ~keep
    fs::write(roots[0].join("f2.rs"), source(2, "edited_main")).expect("write");
    fs::remove_file(roots[0].join("f6.rs")).expect("remove");
    drop(scan_working(&roots[0], &config));
    // An incremental pass leaves linked's old f1 for a full pass to collect. ~keep
    fs::write(roots[1].join("f1.rs"), source(1, "edited_linked")).expect("write");
    let mut linked = Store::open(&roots[1], VIEW_WORKING).expect("open");
    basemind::scanner::scan_paths(
        &roots[1],
        &mut linked,
        &config,
        &[roots[1].join("f1.rs")],
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("incremental pass");
    drop(linked);
    // The pruned worktree is deleted and the reaper removes its workspace. ~keep
    let pruned_cache = basemind::store_layout::workspace_cache_dir(&roots[2]);
    git(
        &["worktree", "remove", "--force", roots[2].to_str().expect("utf-8")],
        &roots[0],
    );
    fs::remove_dir_all(&pruned_cache).expect("reap the workspace");

    let unswept = group_rows(&Store::open(&roots[0], VIEW_WORKING).expect("open"));
    let main = scan_working(&roots[0], &config);
    let swept = group_rows(&main);
    drop(main);

    let mirrored = mirror_group(&[&roots[0], &roots[1]], &tmp.path().join("fresh"));
    let fresh: Vec<Store> = mirrored.iter().map(|root| scan_working(root, &config)).collect();
    let expected = group_rows(&fresh[0]);

    for (name, rows) in &expected.shared {
        let before = unswept.shared[name].len();
        let after = swept.shared[name].len();
        assert!(
            swept.shared[name] == *rows,
            "{name}: {after} rows after the sweep ({before} before), {} in a fresh group",
            rows.len()
        );
    }
    assert!(
        unswept.shared["symbols_by_path"].len() > swept.shared["symbols_by_path"].len(),
        "the fixture must leave dead contents for the sweep"
    );
    assert_eq!(swept.views_with_rows.len(), 2, "the removed worktree's rows are gone");
    assert_eq!(expected.views_with_rows.len(), 2);

    for (i, root) in roots[..2].iter().enumerate() {
        let view = Store::open(root, VIEW_WORKING).expect("open");
        let alone = scan_working(&standalone_copy(root, &tmp.path().join(format!("swept-{i}"))), &config);
        assert_equivalent(&view, &alone, &format!("view {i} after the group sweep"));
    }
}
