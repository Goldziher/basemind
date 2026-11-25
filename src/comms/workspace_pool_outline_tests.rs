//! The decoded-outline cache a pool's hosted read stacks share: the worktrees of one repository
//! read one cache, decode the files they hold in common once, and release it when both stacks go.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{DEFAULT_HOT_CAP, WorkspacePool, hosted_stack};

fn git(dir: &Path, args: &[&str]) {
    let status = crate::testing::git_command()
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} in {dir:?}");
}

/// A repository and one linked worktree of it, both checked out at one commit of three sources.
fn repository_with_a_worktree(tmp: &Path) -> (PathBuf, PathBuf) {
    let main = tmp.join("main");
    std::fs::create_dir(&main).expect("create main");
    git(&main, &["init", "-q", "-b", "main"]);
    for name in ["alpha", "beta", "gamma"] {
        let source = format!("pub fn {name}() -> u32 {{ 1 }}\n");
        std::fs::write(main.join(format!("{name}.rs")), source).expect("write source");
    }
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let linked = tmp.join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().expect("utf-8 path"),
            "-b",
            "linked",
        ],
    );
    (main, linked)
}

/// Both worktrees' stacks serve their outlines from one cache, and the stack built second decodes
/// none of the content the first already decoded. Each stack opened a cache of its own before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worktrees_of_a_repository_share_one_outline_cache() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (main, linked) = repository_with_a_worktree(tmp.path());
    let pool = Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    for root in [&main, &linked] {
        pool.rescan(root, None, false, false, &crate::scanner::ScanCancel::default())
            .expect("scan worktree");
    }

    let main_stack = hosted_stack(&pool, &main).await;
    let main_map = main_stack.workspace.cache.load_full();
    main_map.for_each(|_, _| {});
    let decoded = main_map.l1_cache().misses();
    assert!(decoded > 0, "the main worktree decoded its outlines");

    let linked_stack = hosted_stack(&pool, &linked).await;
    let linked_map = linked_stack.workspace.cache.load_full();
    assert_eq!(linked_map.len(), main_map.len(), "both worktrees index the same files");
    assert!(
        std::ptr::eq(main_map.l1_cache(), linked_map.l1_cache()),
        "one outline cache serves both worktrees"
    );
    linked_map.for_each(|_, _| {});
    assert_eq!(
        linked_map.l1_cache().misses(),
        decoded,
        "the linked worktree decodes nothing the main one did"
    );
}

/// The shared cache is released with the last stack that reads it, not kept for the pool's life.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shared_outline_cache_goes_with_the_last_hosted_stack() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (main, linked) = repository_with_a_worktree(tmp.path());
    let pool = Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    for root in [&main, &linked] {
        pool.rescan(root, None, false, false, &crate::scanner::ScanCancel::default())
            .expect("scan worktree");
        drop(hosted_stack(&pool, root).await);
    }
    assert_eq!(pool.shared_outlines().live(), 1, "both stacks read one cache");

    assert_eq!(pool.shed_read_stacks().await, 2);
    // A shed stack's watcher thread lets go of it on its own schedule. ~keep
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while pool.shared_outlines().live() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the shared outline cache outlived every stack"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
