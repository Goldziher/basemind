//! `basemind sessions` and removed worktrees (issue #162, ADR-0049): a worktree whose gitdir is gone
//! and that git no longer lists is removed, and its sessions are this repository's when it was in a
//! directory that holds the repository's worktrees and no other repository.

use std::path::{Path, PathBuf};

use basemind::sessions::Host;

use super::{Env, git, place, report, session, slug};

/// Add the worktree `name` of `env.repo` under `dir` on a branch of the same name.
fn add_worktree(env: &Env, dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    git(
        &env.repo,
        &["worktree", "add", "-q", "-b", name, &path.to_string_lossy()],
    );
    path
}

/// `wts` holds the live worktree `live`, the removed `gone`, the directory `pruned` whose gitdir was
/// deleted, and a plain directory, which is no worktree.
#[test]
fn a_removed_worktrees_sessions_count_for_this_repository() {
    let env = super::setup();
    let wts = env.repo.parent().expect("base").join("wts");
    add_worktree(&env, &wts, "live");
    let gone = add_worktree(&env, &wts, "gone");
    git(&env.repo, &["worktree", "remove", &gone.to_string_lossy()]);
    let pruned = add_worktree(&env, &wts, "pruned");
    std::fs::remove_dir_all(env.repo.join(".git/worktrees/pruned")).expect("delete the gitdir");
    let plain = wts.join("plain");
    std::fs::create_dir_all(&plain).expect("mkdir");
    let claude = env.home.join(".claude/projects");
    for (dir, id) in [(&gone, "r-gone"), (&pruned, "r-pruned"), (&plain, "r-plain")] {
        let project = claude.join(slug(&dir.join("src")));
        place(
            &env,
            dir,
            "claude/removed-worktree.jsonl",
            &project.join(format!("{id}.jsonl")),
        );
    }
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(
        &env,
        &gone,
        "codex/removed-worktree.jsonl",
        &day.join("rollout-gone.jsonl"),
    );
    let report = report(&env, None);
    let removed: Vec<(Host, &str, &str)> = report
        .sessions
        .iter()
        .filter(|s| s.started.as_deref() == Some("2026-11-18T15:30:00.000Z"))
        .map(|s| (s.host, s.id.as_str(), s.checkout.as_str()))
        .collect();
    assert_eq!(
        removed,
        vec![
            (Host::Claude, "r-gone", "gone"),
            (Host::Claude, "r-pruned", "pruned"),
            (Host::Codex, "rollout-gone", "gone"),
        ]
    );
    let r_gone = &session(&report, "r-gone").counts;
    assert_eq!(
        (
            r_gone.read_whole,
            r_gone.edit,
            r_gone.edits_without_read,
            r_gone.outside_paths
        ),
        (1, 1, 0, 0),
        "the absolute read and the edit relative to src/ are both the removed worktree's src/lib.rs"
    );
    let codex = &session(&report, "rollout-gone").counts;
    assert_eq!((codex.read_whole, codex.outside_paths), (1, 0));
    assert_eq!(
        report.other_directories, 5,
        "the four of setup, and the plain directory"
    );
}

/// A directory that holds another repository beside this one's worktrees is not a worktree directory:
/// a gone directory there may have been anything.
#[test]
fn a_directory_holding_another_repository_has_no_removed_worktrees() {
    let env = super::setup();
    let mixed = env.repo.parent().expect("base").join("mixed");
    add_worktree(&env, &mixed, "live");
    let gone = add_worktree(&env, &mixed, "gone");
    git(&env.repo, &["worktree", "remove", &gone.to_string_lossy()]);
    let foreign = mixed.join("foreign");
    std::fs::create_dir_all(&foreign).expect("mkdir");
    git(&foreign, &["init", "-q"]);
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(
        &env,
        &gone,
        "codex/removed-worktree.jsonl",
        &day.join("rollout-gone.jsonl"),
    );
    let report = report(&env, None);
    assert!(report.sessions.iter().all(|s| s.id != "rollout-gone"));
    assert_eq!(report.other_directories, 5, "the four of setup, and the gone directory");
}
