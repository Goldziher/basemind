//! WS3: linked git worktrees share ONE content-addressed blob cache (extract + embed once), and
//! auto-GC is disabled while a shared cache exists so one worktree can't reap another's blobs.

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::config::ConfigV1;
use basemind::scanner::{ScanSource, scan};
use basemind::store::Store;

fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn canon(p: &Path) -> std::path::PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[test]
fn linked_worktrees_share_the_blob_cache() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir(&main).unwrap();

    git(&["init", "-q", "-b", "main"], &main);
    git(&["config", "user.email", "t@example.com"], &main);
    git(&["config", "user.name", "Test"], &main);
    fs::write(main.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    git(&["add", "."], &main);
    git(&["commit", "-qm", "init"], &main);

    let cfg = ConfigV1::with_defaults();

    // Scan the main worktree BEFORE any linked worktree exists: not shared, blobs land locally.
    {
        let mut store = Store::open(&main, basemind::store::VIEW_WORKING).unwrap();
        assert!(!store.blobs_shared, "no linked worktrees yet → not shared");
        scan(&main, &mut store, &cfg, ScanSource::WorkingTree).unwrap();
    }
    let main_blobs = main.join(".basemind").join("blobs");
    assert!(main_blobs.is_dir(), "main scan wrote blobs locally");

    // Add a linked worktree on a new branch.
    let wt2 = tmp.path().join("wt2");
    git(
        &["worktree", "add", "-q", "-b", "feature", wt2.to_str().unwrap()],
        &main,
    );

    // The linked worktree resolves its blob dir to the MAIN worktree's, and marks the cache shared.
    {
        let store2 = Store::open(&wt2, basemind::store::VIEW_WORKING).unwrap();
        assert!(store2.blobs_shared, "linked worktree marks the blob cache shared");
        assert_eq!(
            canon(&store2.blobs_dir),
            canon(&main_blobs),
            "linked worktree blob dir resolves to the main worktree's blobs/"
        );
    }

    // The MAIN worktree, re-opened now that a linked worktree exists, ALSO sees the cache as shared
    // (so its own auto-GC is disabled — it must not reap blobs the linked worktree references).
    {
        let store_main = Store::open(&main, basemind::store::VIEW_WORKING).unwrap();
        assert!(
            store_main.blobs_shared,
            "main worktree observes the linked worktree too"
        );
    }

    // Scanning the linked worktree writes NO blobs into its own dir — the identical a.rs content
    // reuses the blob already in the shared cache.
    {
        let mut store2 = Store::open(&wt2, basemind::store::VIEW_WORKING).unwrap();
        scan(&wt2, &mut store2, &cfg, ScanSource::WorkingTree).unwrap();
    }
    let wt2_local_blobs = wt2.join(".basemind").join("blobs");
    let wt2_local_count = fs::read_dir(&wt2_local_blobs).map(|d| d.count()).unwrap_or(0);
    assert_eq!(
        wt2_local_count, 0,
        "linked worktree wrote no blobs into its own dir; they share the main cache"
    );
    assert!(
        fs::read_dir(&main_blobs).unwrap().count() >= 1,
        "the shared blob dir holds the extraction blobs"
    );
}
