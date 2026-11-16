use super::*;
use crate::test_support::spec;

fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs.iter().map(|(name, value)| (name.into(), value.into())).collect()
}

#[test]
fn the_child_environment_is_the_allow_list_plus_additions_and_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::Codex, root.path());
    spec.env.insert("OPENAI_API_KEY".to_owned(), "from-the-spec".to_owned());
    let parent = os(&[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/dev"),
        ("ANTHROPIC_API_KEY", "sk-parent"),
        ("OPENAI_API_KEY", "sk-parent"),
        ("AWS_SECRET_ACCESS_KEY", "parent-secret"),
        ("GITHUB_TOKEN", "ghp-parent"),
        ("BASEMIND_AGENT_ID", "the-launcher"),
        ("BASEMIND_HOST", "claude"),
        ("BASEMIND_COMMS_DIR", "/launcher/comms"),
        ("BASEMIND_DATA_HOME", "/data"),
        ("path", "/lowercase-is-another-variable"),
    ]);
    let env = spec.child_env(parent);
    let expected: BTreeMap<String, OsString> = [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/dev"),
        ("OPENAI_API_KEY", "from-the-spec"),
        ("BASEMIND_AGENT_ID", "launched-agent"),
        ("BASEMIND_HOST", "codex"),
        ("BASEMIND_DATA_HOME", "/data"),
        ("BASEMIND_COMMS_DIR", "/launcher/comms"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.into()))
    .collect();
    assert_eq!(env, expected);
}

#[test]
fn an_addition_cannot_replace_the_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::Codex, root.path());
    spec.env
        .insert("BASEMIND_AGENT_ID".to_owned(), "someone-else".to_owned());
    let env = spec.child_env(Vec::new());
    assert_eq!(env["BASEMIND_AGENT_ID"], "launched-agent");
}

#[test]
fn environment_names_no_process_can_carry_are_refused() {
    let root = tempfile::tempdir().unwrap();
    for name in ["", "A=B", "NUL\0"] {
        let mut spec = spec(HarnessKind::Codex, root.path());
        spec.env.insert(name.to_owned(), "value".to_owned());
        assert!(
            matches!(spec.prepare(), Err(LaunchError::InvalidEnvName(_))),
            "{name:?} was accepted"
        );
    }
}

#[test]
fn the_working_directory_must_resolve_inside_the_root() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repo");
    let inside = root.join("crates/app");
    let sibling = outer.path().join("elsewhere");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let canonical_root = root.canonicalize().unwrap();

    let mut spec = spec(HarnessKind::Codex, &root);
    assert_eq!(spec.prepare().unwrap().cwd, canonical_root);

    spec.working_dir = Some("crates/app".into());
    assert_eq!(spec.prepare().unwrap().cwd, canonical_root.join("crates/app"));
    spec.working_dir = Some(inside.clone());
    assert_eq!(spec.prepare().unwrap().cwd, canonical_root.join("crates/app"));

    for escape in [
        PathBuf::from(".."),
        PathBuf::from("crates/../../elsewhere"),
        sibling.clone(),
    ] {
        spec.working_dir = Some(escape.clone());
        assert!(
            matches!(spec.prepare(), Err(LaunchError::WorkingDirOutsideRoot { .. })),
            "{} was accepted",
            escape.display()
        );
    }

    spec.working_dir = Some("missing".into());
    assert!(matches!(spec.prepare(), Err(LaunchError::Path { .. })));
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_root_does_not_widen_the_confinement() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repo");
    let sibling = outer.path().join("elsewhere");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::os::unix::fs::symlink(&sibling, root.join("link")).unwrap();
    let mut spec = spec(HarnessKind::Codex, &root);
    spec.working_dir = Some("link".into());
    assert!(matches!(spec.prepare(), Err(LaunchError::WorkingDirOutsideRoot { .. })));
}

#[test]
fn zero_limits_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::Codex, root.path());
    spec.limits.max_turns = 0;
    assert!(matches!(spec.prepare(), Err(LaunchError::InvalidLimits(_))));
    spec.limits.max_turns = 1;
    spec.limits.wall_time = Duration::ZERO;
    assert!(matches!(spec.prepare(), Err(LaunchError::InvalidLimits(_))));
}

#[test]
fn the_guard_finds_a_bypass_in_any_spelling_and_passes_ordinary_arguments() {
    let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    for refused in [
        args(&["--dangerously-skip-permissions"]),
        args(&["--allow-dangerously-skip-permissions"]),
        args(&["--permission-mode", "bypassPermissions"]),
        args(&["--dangerously-bypass-approvals-and-sandbox"]),
        args(&["--sandbox", "danger-full-access"]),
        args(&["-c", "sandbox_mode=\"danger-full-access\""]),
        args(&["--yolo"]),
        args(&["agent", "--always-approve", "stdio"]),
    ] {
        assert!(
            guard_arguments(&refused, false).is_err(),
            "{refused:?} passed the guard"
        );
        assert!(guard_arguments(&refused, true).is_ok());
    }
    let ordinary = args(&["--model", "sonnet", "--permission-mode", "plan", "acp", "--verbose"]);
    assert!(guard_arguments(&ordinary, false).is_ok());
}

#[test]
fn the_default_spec_is_supervised_single_turn_and_denies() {
    let root = tempfile::tempdir().unwrap();
    let spec = spec(HarnessKind::ClaudeCode, root.path());
    assert_eq!(spec.permission_mode, PermissionMode::Supervised);
    assert_eq!(spec.limits.max_turns, 1);
    assert!(spec.bypass.is_none());
    assert!(spec.approval_policy.is_none());
    assert!(spec.mcp_server.is_none());
    let prepared = spec.prepare().unwrap();
    assert!(!prepared.bypass(&spec));
}
