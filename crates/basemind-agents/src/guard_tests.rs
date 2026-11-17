use std::path::PathBuf;

use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn the_bypass_guard_passes_only_a_launch_that_runs_in_bypass() {
    for list in [
        args(&["--dangerously-skip-permissions"]),
        args(&["--permission-mode", "bypassPermissions"]),
        args(&["-c", "sandbox_mode=\"danger-full-access\""]),
        args(&["agent", "--always-approve", "stdio"]),
    ] {
        assert!(guard_bypass(&list, false).is_err(), "{list:?} passed the guard");
        assert!(guard_bypass(&list, true).is_ok());
    }
    assert!(guard_bypass(&args(&["--permission-mode", "plan", "acp"]), false).is_ok());
}

#[test]
fn environment_additions_are_limited_to_provider_credentials_and_settings() {
    for name in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GITHUB_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "AWS_REGION",
        "GOOGLE_CLOUD_PROJECT",
    ] {
        assert!(check_env_name(name).is_ok(), "{name} was refused");
    }
    for name in [
        "",
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
        "A=B_TOKEN",
        "NUL\0_TOKEN",
        "lower_api_key",
        "1_TOKEN",
        "PATH",
        "HOME",
        "XDG_CONFIG_HOME",
        "TMPDIR",
        "SYSTEMROOT",
        "BASEMIND_DATA_HOME",
        "BASEMIND_COMMS_DIR",
        "BASEMIND_AGENT_ID",
        "BASEMIND_HOST",
        "BASEMIND_SERVER_TOKEN",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "NODE_OPTIONS",
        "NODE_AUTH_TOKEN",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "OPENCODE_CONFIG",
        "GIT_SSH_COMMAND",
        "SHELL",
        "EDITOR",
        "HTTPS_PROXY",
    ] {
        assert!(
            matches!(check_env_name(name), Err(LaunchError::EnvNotAllowed(_))),
            "{name:?} was accepted"
        );
    }
}

#[test]
fn a_harness_project_file_in_the_working_directory_or_above_refuses_the_launch() {
    let outer = tempfile::tempdir().unwrap();
    let outer = outer.path().canonicalize().unwrap();
    let cwd = outer.join("repo/crates/app");
    std::fs::create_dir_all(&cwd).unwrap();
    let refused = |kind, home: Option<&Path>| match check_project_config(kind, &cwd, home) {
        Err(LaunchError::ProjectConfig { path, .. }) => Some(path),
        Ok(()) => None,
        Err(other) => panic!("unexpected error {other}"),
    };
    for kind in [
        HarnessKind::Codex,
        HarnessKind::Acp(AcpFlavor::Gemini),
        HarnessKind::Acp(AcpFlavor::Cursor),
        HarnessKind::Acp(AcpFlavor::OpenCode),
    ] {
        assert_eq!(refused(kind, None), None, "{kind} refused a clean tree");
    }

    std::fs::create_dir_all(outer.join("repo/.codex")).unwrap();
    std::fs::write(outer.join("repo/.codex/config.toml"), "").unwrap();
    std::fs::create_dir_all(cwd.join(".gemini")).unwrap();
    std::fs::create_dir_all(outer.join(".cursor")).unwrap();
    std::fs::write(outer.join(".cursor/cli.json"), "{}").unwrap();
    std::fs::write(outer.join("repo/opencode.json"), "{}").unwrap();
    std::fs::create_dir_all(cwd.join(".claude")).unwrap();
    std::fs::write(cwd.join(".mcp.json"), "{}").unwrap();

    assert_eq!(refused(HarnessKind::Codex, None), Some(outer.join("repo/.codex")));
    assert_eq!(
        refused(HarnessKind::Acp(AcpFlavor::Gemini), None),
        Some(cwd.join(".gemini"))
    );
    assert_eq!(
        refused(HarnessKind::Acp(AcpFlavor::Cursor), None),
        Some(outer.join(".cursor/cli.json"))
    );
    std::fs::remove_file(outer.join(".cursor/cli.json")).unwrap();
    assert_eq!(
        refused(HarnessKind::Acp(AcpFlavor::OpenCode), None),
        Some(outer.join("repo/opencode.json"))
    );
    // Claude Code's project sources are switched off by flag instead.
    assert_eq!(refused(HarnessKind::ClaudeCode, None), None);
    // The user's own configuration lives under the same names in the home directory.
    assert_eq!(refused(HarnessKind::Acp(AcpFlavor::Cursor), Some(&outer)), None);
    assert_eq!(
        refused(HarnessKind::Codex, Some(&PathBuf::from(&outer))),
        Some(outer.join("repo/.codex"))
    );
}

#[test]
fn cursor_may_find_only_its_rules_and_an_unreadable_place_counts_as_present() {
    let outer = tempfile::tempdir().unwrap();
    let outer = outer.path().canonicalize().unwrap();
    let cwd = outer.join("repo");
    std::fs::create_dir_all(cwd.join(".cursor/rules")).unwrap();
    let cursor = HarnessKind::Acp(AcpFlavor::Cursor);
    assert!(check_project_config(cursor, &cwd, None).is_ok());
    for name in ["hooks.json", "anything-new.json"] {
        std::fs::write(cwd.join(".cursor").join(name), "{}").unwrap();
        assert!(
            matches!(
                check_project_config(cursor, &cwd, None),
                Err(LaunchError::ProjectConfig { path, .. }) if path == cwd.join(".cursor").join(name)
            ),
            "{name} was not refused"
        );
        std::fs::remove_file(cwd.join(".cursor").join(name)).unwrap();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = outer.join("locked");
        std::fs::create_dir_all(locked.join("work")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = check_project_config(HarnessKind::Codex, &locked.join("work"), None);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(result, Err(LaunchError::ProjectConfig { .. })),
            "a directory that cannot be searched passed: {result:?}"
        );
    }
}

#[test]
fn a_root_at_or_above_home_or_around_a_configuration_directory_is_refused() {
    let outer = tempfile::tempdir().unwrap();
    let outer = outer.path().canonicalize().unwrap();
    let home = outer.join("home/user");
    let data = outer.join("data");
    let comms = outer.join("comms");
    for dir in [
        home.join("src/repo"),
        home.join(".codex/memories"),
        data.clone(),
        comms.clone(),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let env = |name: &str| match name {
        "HOME" => Some(home.clone().into_os_string()),
        "BASEMIND_DATA_HOME" => Some(data.clone().into_os_string()),
        "BASEMIND_COMMS_DIR" => Some(comms.clone().into_os_string()),
        _ => None,
    };
    assert!(check_root(&home.join("src/repo"), &env).is_ok());
    for root in [
        outer.clone(),
        outer.join("home"),
        home.clone(),
        home.join(".codex"),
        home.join(".codex/memories"),
        data.clone(),
        comms.clone(),
    ] {
        assert!(
            matches!(check_root(&root, &env), Err(LaunchError::RootNotAllowed { .. })),
            "{} was accepted",
            root.display()
        );
    }
}

#[test]
fn a_dotenv_file_refuses_a_harness_that_may_load_it() {
    let outer = tempfile::tempdir().unwrap();
    let outer = outer.path().canonicalize().unwrap();
    let cwd = outer.join("repo/app");
    std::fs::create_dir_all(&cwd).unwrap();
    for name in [".env", ".env.local"] {
        let file = outer.join("repo").join(name);
        std::fs::write(&file, "GEMINI_API_KEY=x\n").unwrap();
        for flavor in [
            AcpFlavor::Gemini,
            AcpFlavor::Cursor,
            AcpFlavor::Grok,
            AcpFlavor::Hermes,
            AcpFlavor::Generic,
        ] {
            let kind = HarnessKind::Acp(flavor);
            assert!(
                matches!(
                    check_project_config(kind, &cwd, None),
                    Err(LaunchError::ProjectConfig { ref path, .. }) if *path == file
                ),
                "{kind} launched over {name}"
            );
        }
        // Verified live not to read a working directory's `.env`.
        for kind in [
            HarnessKind::ClaudeCode,
            HarnessKind::Codex,
            HarnessKind::Acp(AcpFlavor::OpenCode),
        ] {
            assert!(check_project_config(kind, &cwd, None).is_ok(), "{kind} refused {name}");
        }
        std::fs::remove_file(&file).unwrap();
    }
    std::fs::write(outer.join("repo/environment.txt"), "").unwrap();
    assert!(check_project_config(HarnessKind::Acp(AcpFlavor::Gemini), &cwd, None).is_ok());
}
