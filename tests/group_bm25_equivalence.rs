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
    scan(root, &mut store, config, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan");
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
