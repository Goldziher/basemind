use super::*;

fn repo(config: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".git/config"), config).unwrap();
    (outer, root)
}

#[test]
fn a_repository_config_that_runs_commands_on_a_read_refuses_the_launch() {
    for (config, key) in [
        ("[filter \"probe\"]\n\tclean = ./x.sh\n", "filter.*.clean"),
        (
            "[Filter \"lfs\"]\n  Process = git-lfs filter-process\n",
            "filter.*.process",
        ),
        ("[diff \"bin\"]\ntextconv = ./dump\n", "diff.*.textconv"),
        ("[diff]\n\texternal = ./difftool\n", "diff.external"),
        ("[include]\n\tpath = ../other.config\n", "include.path"),
        ("[includeIf \"gitdir:/x/\"]\n\tpath = x\n", "includeif.*.path"),
        ("[core] fsmonitor = ./a\n[filter.x] smudge = y\n", "filter.*.smudge"),
    ] {
        let (_outer, root) = repo(config);
        let error = check_repository(&root.join("src")).unwrap_err();
        let LaunchError::GitConfig { key: found, .. } = &error else {
            panic!("{config:?}: {error}");
        };
        assert_eq!(found, key, "{config:?}");
    }
}

#[test]
fn an_ordinary_repository_config_passes() {
    let config = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tfsmonitor = ./hook\n\
                  # [filter \"x\"] clean = y\n[remote \"origin\"]\n\turl = git@example.org:a/b\n\
                  [branch \"main\"]\n\tremote = origin\n[diff]\n\tcolorMoved = zebra\n";
    let (_outer, root) = repo(config);
    assert!(check_repository(&root.join("src")).is_ok());
    let outside = tempfile::tempdir().unwrap();
    assert!(check_repository(outside.path()).is_ok());
}

#[test]
fn a_worktree_is_checked_through_its_common_directory() {
    let (outer, main) = repo("[filter \"probe\"]\n\tclean = ./x.sh\n");
    let gitdir = main.join(".git/worktrees/feature");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    let worktree = outer.path().join("feature");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    assert!(matches!(
        check_repository(&worktree),
        Err(LaunchError::GitConfig { .. })
    ));
    std::fs::write(main.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    std::fs::write(gitdir.join("config.worktree"), "[diff]\n\texternal = x\n").unwrap();
    assert!(matches!(
        check_repository(&worktree),
        Err(LaunchError::GitConfig { .. })
    ));
}

#[test]
fn every_harness_is_refused_such_a_repository() {
    let (_outer, root) = repo("[filter \"probe\"]\n\tclean = ./x.sh\n");
    for kind in [crate::HarnessKind::ClaudeCode, crate::HarnessKind::Codex] {
        let result = crate::test_support::spec(kind, &root).prepare();
        assert!(matches!(result, Err(LaunchError::GitConfig { .. })), "{kind}");
    }
}
