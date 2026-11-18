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
        ("[gpg]\n\tprogram = ./sign\n", "gpg.program"),
        ("[gpg \"ssh\"]\n\tprogram = ./sign\n", "gpg.*.program"),
        ("[gpg.x509]\n\tprogram = ./sign\n", "gpg.*.program"),
    ] {
        let (_outer, root) = repo(config);
        let error = check_repository(&root.join("src")).unwrap_err();
        let LaunchError::GitConfig { key: found, .. } = &error else {
            panic!("{config:?}: {error}");
        };
        assert_eq!(found, key, "{config:?}");
    }
}

/// Syntax git 2.x accepts and runs the filter of (checked with `git add`): a byte-order mark
/// before the first header, a `]` or an escaped quote inside a quoted subsection with the key
/// on the header line, two headers on one line, and a lone carriage return (a space to git, not
/// a line end) between the section and its quoted subsection.
#[test]
fn header_syntax_git_accepts_cannot_hide_a_command_key() {
    for (config, key) in [
        ("\u{feff}[filter \"x\"]\n\tclean = ./x.sh\n", "filter.*.clean"),
        ("[filter \"x]y\"] clean = ./x.sh\n", "filter.*.clean"),
        ("[filter \"a\\\"]b\"] smudge = ./x.sh\n", "filter.*.smudge"),
        ("[core] [filter \"z\"] clean = ./x.sh\n", "filter.*.clean"),
        ("[diff \"q\\\\\"]textconv=./d\n", "diff.*.textconv"),
        ("[filter\r\"x\"]\n\tclean = ./evil\n", "filter.*.clean"),
        ("[filter \r\t\"x\"]\n\tclean = ./evil\n", "filter.*.clean"),
    ] {
        let (_outer, root) = repo(config);
        let error = check_repository(&root.join("src")).expect_err(config);
        let LaunchError::GitConfig { key: found, .. } = &error else {
            panic!("{config:?}: {error}");
        };
        assert_eq!(found, key, "{config:?}");
    }
}

/// A header this check cannot parse refuses the launch rather than hiding the keys under it:
/// git refuses such a file, unless the header is one it reads and this check does not.
#[test]
fn an_unparseable_section_header_refuses_the_launch() {
    for config in [
        "[filter\u{b}\"x\"]\n\tclean = ./evil\n",
        "[filter \"x\n\tclean = ./evil\n",
        "[core]\n[filter \"x\" junk]\n\tclean = ./evil\n",
    ] {
        let (_outer, root) = repo(config);
        let error = check_repository(&root.join("src")).expect_err(config);
        let LaunchError::GitConfig { key, .. } = &error else {
            panic!("{config:?}: {error}");
        };
        assert_eq!(key, "an unparseable section header", "{config:?}");
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
