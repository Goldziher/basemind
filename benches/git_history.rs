//! Git-history read-path microbenchmarks.
//!
//! Builds a synthetic git repo (~`COMMITS` commits) in a tempdir, indexes it once via
//! `GitHistoryIndex::open` + `builder::sync`, then benches the posting-list reader the history MCP
//! tools sit on against the live `gix` walk it replaces. The indexed path should be orders of
//! magnitude faster; these benches are the before/after gate for the read-path optimization work.
//!
//! Point `BASEMIND_BENCH_REPO` at a repo whose `.basemind/git-history.fjall` is already built to
//! profile against real history; the synthetic repo is used otherwise so the bench is
//! self-contained and CI-safe.

use std::path::Path;
use std::process::Command;

use basemind::git::Repo;
use basemind::git_history::GitHistoryIndex;
use basemind::git_history::builder;
use basemind::path::RelPath;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tempfile::TempDir;

/// Synthetic history depth. Deep enough that the live walk has real work to do (the index's win
/// grows with depth) while keeping `setup` to a couple of seconds of `git` subprocess time.
const COMMITS: usize = 400;
/// How many distinct "rare" files exist; each is touched by exactly one commit.
const RARE_FILES: usize = 120;

/// A handle to a built index plus the inputs the benches query.
struct Harness {
    _dir: Option<TempDir>,
    repo: Repo,
    index: GitHistoryIndex,
    hot_path: RelPath,
    rare_path: RelPath,
}

fn git(repo: &Path, args: &[&str], date: &str) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "bench")
        .env("GIT_AUTHOR_EMAIL", "bench@e.x")
        .env("GIT_COMMITTER_NAME", "bench")
        .env("GIT_COMMITTER_EMAIL", "bench@e.x")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// Build a synthetic repo: every commit touches `hot.rs`; every `STRIDE`th commit also creates a
/// fresh `rare_<n>.rs` touched exactly once. Returns the built harness.
fn build_synthetic() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"], "2020-01-01T00:00:00");
    git(root, &["config", "commit.gpgsign", "false"], "2020-01-01T00:00:00");

    let stride = COMMITS / RARE_FILES.max(1);
    for i in 0..COMMITS {
        let date = format!("2020-01-01T00:{:02}:{:02}", (i / 60) % 60, i % 60);
        std::fs::write(root.join("hot.rs"), format!("fn hot() {{ /* rev {i} */ }}\n")).unwrap();
        git(root, &["add", "hot.rs"], &date);
        if i % stride == 0 {
            let name = format!("rare_{i}.rs");
            std::fs::write(root.join(&name), format!("fn rare_{i}() {{}}\n")).unwrap();
            git(root, &["add", &name], &date);
        }
        git(root, &["commit", "-qm", &format!("c{i}")], &date);
    }

    let bdir = root.join(".basemind");
    std::fs::create_dir_all(&bdir).unwrap();
    let repo = Repo::discover(root).expect("discover");
    let index = GitHistoryIndex::open(&bdir).expect("open index");
    builder::sync(&index, &repo, &bdir).expect("sync");
    Harness {
        _dir: Some(dir),
        repo,
        index,
        hot_path: RelPath::from("hot.rs"),
        rare_path: RelPath::from("rare_0.rs"),
    }
}

/// Open a prebuilt real-repo index pointed at by `BASEMIND_BENCH_REPO`, or `None` to use the
/// synthetic repo. Samples a hot + rare path from the newest commits.
fn open_real(repo_root: &str) -> Option<Harness> {
    let root = Path::new(repo_root);
    let bdir = root.join(".basemind");
    if !bdir.join("git-history.fjall").exists() {
        eprintln!("BASEMIND_BENCH_REPO={repo_root} has no git-history.fjall; using synthetic repo");
        return None;
    }
    let repo = Repo::discover(root).ok()?;
    let index = GitHistoryIndex::open(&bdir).ok()?;
    let window = index.window_commits(2000);
    let mut counts: ahash::AHashMap<RelPath, usize> = ahash::AHashMap::new();
    for commit in &window {
        for (rel, _) in &commit.files {
            *counts.entry(rel.clone()).or_default() += 1;
        }
    }
    let hot_path = counts.iter().max_by_key(|(_, n)| **n).map(|(p, _)| p.clone())?;
    let rare_path = counts
        .iter()
        .find(|(_, n)| **n == 1)
        .map(|(p, _)| p.clone())
        .unwrap_or_else(|| hot_path.clone());
    Some(Harness {
        _dir: None,
        repo,
        index,
        hot_path,
        rare_path,
    })
}

fn harness() -> Harness {
    match std::env::var("BASEMIND_BENCH_REPO") {
        Ok(repo) => open_real(&repo).unwrap_or_else(build_synthetic),
        Err(_) => build_synthetic(),
    }
}

fn bench_git_history(c: &mut Criterion) {
    let h = harness();

    let mut group = c.benchmark_group("commits_touching");
    group.bench_function("indexed_hot", |b| {
        b.iter(|| black_box(h.index.commits_touching(black_box(&h.hot_path), 0, 50)))
    });
    group.bench_function("livewalk_hot", |b| {
        b.iter(|| black_box(h.repo.log_for_path(black_box(&h.hot_path), 50).unwrap()))
    });
    group.bench_function("indexed_rare", |b| {
        b.iter(|| black_box(h.index.commits_touching(black_box(&h.rare_path), 0, 50)))
    });
    group.bench_function("livewalk_rare", |b| {
        b.iter(|| black_box(h.repo.log_for_path(black_box(&h.rare_path), 50).unwrap()))
    });
    group.finish();

    let mut group = c.benchmark_group("recent_changes");
    group.bench_function("indexed", |b| {
        b.iter(|| black_box(h.index.recent_commits(0, 50, false)))
    });
    group.bench_function("livewalk", |b| {
        b.iter(|| black_box(h.repo.log_paths(50, false).unwrap()))
    });
    group.finish();

    let mut group = c.benchmark_group("window_commits");
    group.bench_function("indexed_300", |b| b.iter(|| black_box(h.index.window_commits(300))));
    group.finish();
}

criterion_group!(benches, bench_git_history);
criterion_main!(benches);
