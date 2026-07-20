//! WS3: linked git worktrees share ONE content-addressed blob cache (extract + embed once).
//!
//! Since the blob store went machine-global, *every* workspace on the machine shares the single
//! global `blobs/` — so a main worktree and its linked worktree dedup byte-identical files for free
//! (no per-worktree blob dir, no re-embed). `blobs_shared` is now always `true` (auto-GC is
//! disabled in the standalone Store because it can't see other workspaces' references).

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
fn linked_worktrees_share_the_global_blob_cache() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir(&main).unwrap();

    git(&["init", "-q", "-b", "main"], &main);
    git(&["config", "user.email", "t@example.com"], &main);
    git(&["config", "user.name", "Test"], &main);
    fs::write(main.join("a.rs"), b"pub fn worktree_share_alpha() {}\n").unwrap();
    git(&["add", "."], &main);
    git(&["commit", "-qm", "init"], &main);

    let cfg = ConfigV1::with_defaults();
    let global_blobs = basemind::store::global_blobs_dir();

    {
        let mut store = Store::open(&main, basemind::store::VIEW_WORKING).unwrap();
        assert!(
            store.blobs_shared,
            "blobs are machine-global now → the standalone Store always flags shared (auto-GC off)"
        );
        assert_eq!(
            canon(&store.blobs_dir),
            canon(&global_blobs),
            "the store's blob dir is the machine-global store"
        );
        scan(
            &main,
            &mut store,
            &cfg,
            ScanSource::WorkingTree,
            basemind::scanner::EmbedMode::Inline,
        )
        .unwrap();
    }
    let after_main = fs::read_dir(&global_blobs)
        .expect("global blobs dir exists")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".fm.msgpack")))
        .count();
    assert!(
        after_main >= 1,
        "main scan wrote its extraction blob to the global store"
    );

    let wt2 = tmp.path().join("wt2");
    git(
        &["worktree", "add", "-q", "-b", "feature", wt2.to_str().unwrap()],
        &main,
    );

    {
        let store2 = Store::open(&wt2, basemind::store::VIEW_WORKING).unwrap();
        assert_eq!(
            canon(&store2.blobs_dir),
            canon(&global_blobs),
            "the linked worktree resolves to the same global blob store"
        );
    }

    {
        let mut store2 = Store::open(&wt2, basemind::store::VIEW_WORKING).unwrap();
        let report = scan(
            &wt2,
            &mut store2,
            &cfg,
            ScanSource::WorkingTree,
            basemind::scanner::EmbedMode::Inline,
        )
        .unwrap();
        assert_eq!(
            report.stats.reused_extraction, 1,
            "linked worktree reuses the main worktree's blob from the shared global store"
        );
    }

    assert!(
        !main.join(".basemind").exists(),
        "no in-repo cache under the main worktree"
    );
    assert!(
        !wt2.join(".basemind").exists(),
        "no in-repo cache under the linked worktree"
    );
}
