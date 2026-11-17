use super::*;
use crate::test_support::{BIN_DIRS, MCP_COMMAND, config, spec};

fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs.iter().map(|(name, value)| (name.into(), value.into())).collect()
}

#[test]
fn the_child_environment_is_the_allow_list_plus_additions_and_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::Codex, root.path());
    spec.env.insert("OPENAI_API_KEY".to_owned(), "from-the-spec".to_owned());
    let parent = os(&[
        ("PATH", BIN_DIRS[0]),
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
    let mut expected: BTreeMap<String, OsString> = [
        ("PATH", BIN_DIRS[0]),
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
    if cfg!(windows) {
        expected.insert("NoDefaultCurrentDirectoryInExePath".to_owned(), "1".into());
    }
    assert_eq!(env, expected);
}

#[test]
fn additions_that_would_redirect_the_child_are_refused_from_the_spec_and_the_server() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "",
        "A=B",
        "BASEMIND_AGENT_ID",
        "BASEMIND_COMMS_DIR",
        "CLAUDE_CONFIG_DIR",
        "LD_PRELOAD",
        "PATH",
    ] {
        let mut spec = spec(HarnessKind::Codex, root.path());
        spec.env.insert(name.to_owned(), "value".to_owned());
        assert!(
            matches!(spec.prepare(), Err(LaunchError::EnvNotAllowed(_))),
            "{name:?} was accepted from the spec"
        );
        let mut spec = spec_with_server(root.path());
        spec.mcp_server
            .as_mut()
            .unwrap()
            .env
            .insert(name.to_owned(), "value".to_owned());
        assert!(
            matches!(spec.prepare(), Err(LaunchError::EnvNotAllowed(_))),
            "{name:?} was accepted from the server"
        );
    }
    let mut spec = spec_with_server(root.path());
    spec.mcp_server
        .as_mut()
        .unwrap()
        .env
        .insert("SEARCH_API_KEY".to_owned(), "value".to_owned());
    assert!(spec.prepare().is_ok());
}

fn spec_with_server(root: &Path) -> LaunchSpec {
    let mut spec = spec(HarnessKind::Codex, root);
    spec.mcp_server = Some(McpServerSpec::basemind_serve(MCP_COMMAND));
    spec
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
    assert!(matches!(spec.prepare(), Err(LaunchError::WorkingDirOutsideRoot { .. })));
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
fn limits_that_overflow_a_deadline_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::Codex, root.path());
    spec.limits.wall_time = Duration::MAX;
    assert!(matches!(spec.prepare(), Err(LaunchError::InvalidLimits(_))));
    spec.limits.wall_time = Duration::from_secs(60);
    spec.limits.stop_grace = Duration::MAX;
    assert!(matches!(spec.prepare(), Err(LaunchError::InvalidLimits(_))));
    spec.limits.stop_grace = Duration::from_secs(365 * 24 * 3600);
    assert!(spec.prepare().is_ok());
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
fn a_request_cannot_raise_the_caps_or_leave_the_root_by_an_absolute_path() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(HarnessKind::Codex, root.path());
    config.limits = Limits {
        wall_time: Duration::from_secs(60),
        max_turns: 3,
        stop_grace: Duration::from_secs(2),
    };
    let mut request = LaunchRequest::new("Review the diff.");
    assert_eq!(LaunchSpec::new(&config, &request).unwrap().limits, config.limits);

    request.max_turns = Some(1000);
    request.wall_time = Some(Duration::from_secs(24 * 3600));
    assert_eq!(LaunchSpec::new(&config, &request).unwrap().limits, config.limits);

    request.max_turns = Some(2);
    request.wall_time = Some(Duration::from_secs(5));
    let limits = LaunchSpec::new(&config, &request).unwrap().limits;
    assert_eq!((limits.max_turns, limits.wall_time), (2, Duration::from_secs(5)));
    assert_eq!(limits.stop_grace, config.limits.stop_grace);

    for (turns, wall_time) in [(Some(0), None), (None, Some(Duration::ZERO))] {
        let request = LaunchRequest {
            max_turns: turns,
            wall_time,
            ..LaunchRequest::new("x")
        };
        assert!(matches!(
            LaunchSpec::new(&config, &request),
            Err(LaunchError::InvalidLimits(_))
        ));
    }

    let inside = root.path().join("sub");
    std::fs::create_dir(&inside).unwrap();
    let request = LaunchRequest {
        working_dir: Some(inside.clone()),
        ..LaunchRequest::new("x")
    };
    assert!(matches!(
        LaunchSpec::new(&config, &request),
        Err(LaunchError::WorkingDirNotRelative(dir)) if dir == inside
    ));
}

#[test]
fn a_model_and_an_effort_become_the_harnesses_own_flags() {
    let root = tempfile::tempdir().unwrap();
    let with = |kind, model: Option<&str>, effort: Option<&str>| {
        let mut spec = spec(kind, root.path());
        spec.model = model.map(str::to_owned);
        spec.effort = effort.map(str::to_owned);
        spec.prepare().map(|prepared| prepared.model_args)
    };
    assert_eq!(
        with(HarnessKind::ClaudeCode, Some("claude-opus-4-6[1m]"), Some("high")).unwrap(),
        ["--model", "claude-opus-4-6[1m]", "--effort", "high"]
    );
    assert_eq!(
        with(HarnessKind::Codex, Some("gpt-5.1-codex"), Some("high")).unwrap(),
        ["-c", "model=\"gpt-5.1-codex\"", "-c", "model_reasoning_effort=\"high\""]
    );
    assert_eq!(
        with(HarnessKind::Acp(AcpFlavor::Grok), Some("grok-4"), None).unwrap(),
        ["--model", "grok-4"]
    );
    assert!(
        with(HarnessKind::Acp(AcpFlavor::Cursor), None, None)
            .unwrap()
            .is_empty()
    );
    for flavor in [AcpFlavor::Cursor, AcpFlavor::Gemini, AcpFlavor::Generic] {
        assert!(matches!(
            with(HarnessKind::Acp(flavor), Some("model"), None),
            Err(LaunchError::Unsupported { .. })
        ));
    }
    for (model, effort) in [
        (Some("--dangerously-skip-permissions"), None),
        (Some("sonnet --allowedTools Bash"), None),
        (Some("x\"\nsandbox_mode=\"danger-full-access"), None),
        (Some(""), None),
        (Some(&"m".repeat(129)[..]), None),
        (None, Some("HIGH")),
        (None, Some("high max")),
    ] {
        for kind in [HarnessKind::ClaudeCode, HarnessKind::Codex] {
            assert!(
                matches!(with(kind, model, effort), Err(LaunchError::ArgumentNotAllowed { .. })),
                "{kind} accepted {model:?} / {effort:?}"
            );
        }
    }
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

#[cfg(unix)]
#[test]
fn paths_that_are_not_utf8_are_refused_rather_than_serialized() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let outer = tempfile::tempdir().unwrap();
    let odd = std::ffi::OsStr::from_bytes(b"caf\xe9");
    let root = outer.path().join(odd);
    if std::fs::create_dir(&root).is_err() {
        // Some file systems (APFS among them) refuse names that are not UTF-8: then neither
        // a working directory nor a command can carry one, and nothing is left to check.
        assert!(std::str::from_utf8(odd.as_bytes()).is_err());
        return;
    }
    let spec = spec(HarnessKind::Codex, &root);
    assert!(matches!(spec.prepare(), Err(LaunchError::NonUtf8Path(_))));
    let command = outer.path().join("bin").join(odd);
    std::fs::create_dir(outer.path().join("bin")).unwrap();
    std::fs::write(&command, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plain_root = outer.path().join("root");
    std::fs::create_dir(&plain_root).unwrap();
    for kind in [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Acp(crate::harness::AcpFlavor::Cursor),
    ] {
        let mut config = config(kind, &plain_root);
        config.mcp_server = Some(McpServerSpec::basemind_serve(&command));
        assert!(
            matches!(
                crate::Registry::with_defaults().plan(&config, &LaunchRequest::new("x")),
                Err(LaunchError::NonUtf8Path(_))
            ),
            "{kind} accepted the path"
        );
    }
}

#[cfg(unix)]
fn executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn an_executable_is_found_on_absolute_path_entries_and_never_inside_the_root() {
    let outer = tempfile::tempdir().unwrap();
    let outer = outer.path().canonicalize().unwrap();
    let root = outer.join("root");
    let bin = outer.join("bin");
    executable(&bin.join("harness"));
    executable(&root.join("tools/harness"));
    executable(&root.join("harness"));
    let search = std::env::join_paths([Path::new(""), Path::new("tools"), Path::new("."), &bin]).unwrap();
    let resolve = |path: &str| resolve_executable(Path::new(path), Some(&search), &root);
    // The relative entries would find the copies in the root first, were they searched.
    assert_eq!(resolve("harness").unwrap(), bin.join("harness"));
    assert_eq!(
        resolve(bin.join("harness").to_str().unwrap()).unwrap(),
        bin.join("harness")
    );
    let refused = |path: &str| matches!(resolve(path), Err(LaunchError::InvalidExecutable { .. }));
    assert!(refused("missing"));
    assert!(refused("./harness"));
    assert!(refused("tools/harness"));
    assert!(refused(root.join("harness").to_str().unwrap()));
    // A link outside the root that leads into it is refused too.
    std::os::unix::fs::symlink(root.join("harness"), bin.join("linked")).unwrap();
    assert!(refused(bin.join("linked").to_str().unwrap()));
    assert!(refused("linked"));
    // So is a chain that passes through the root and leaves it again: the agent could
    // retarget the middle link.
    std::os::unix::fs::symlink(bin.join("harness"), root.join("hop")).unwrap();
    std::os::unix::fs::symlink(root.join("hop"), bin.join("chained")).unwrap();
    assert!(refused(bin.join("chained").to_str().unwrap()));
    // And a relative link in a directory that is itself a link into the root.
    std::os::unix::fs::symlink(&root, outer.join("alias")).unwrap();
    assert!(refused(outer.join("alias/hop").to_str().unwrap()));
    // A chain that stays outside the root is fine.
    std::os::unix::fs::symlink("harness", bin.join("relative")).unwrap();
    std::os::unix::fs::symlink(bin.join("relative"), outer.join("outside")).unwrap();
    assert_eq!(
        resolve(outer.join("outside").to_str().unwrap()).unwrap(),
        outer.join("outside")
    );
}

#[test]
fn batch_files_are_told_apart_by_their_extension() {
    for name in ["claude.cmd", "CODEX.BAT", "x.Cmd"] {
        assert!(is_batch(Path::new(name)), "{name}");
    }
    for name in ["claude.exe", "claude", "cmd", "x.cmdx"] {
        assert!(!is_batch(Path::new(name)), "{name}");
    }
}

#[test]
fn a_bare_name_is_looked_up_with_each_pathext_extension_unless_it_ends_in_one() {
    let names = |name: &str, pathext: Option<&str>| with_pathext(Path::new(name), pathext.map(std::ffi::OsStr::new));
    assert_eq!(
        names("claude", Some(".EXE;.CMD;")),
        [PathBuf::from("claude.EXE"), PathBuf::from("claude.CMD")]
    );
    assert_eq!(names("codex.exe", Some(".EXE;.CMD")), [PathBuf::from("codex.exe")]);
    assert_eq!(names("python3.11", Some(".EXE")), [PathBuf::from("python3.11.EXE")]);
    assert_eq!(names("acp", None).len(), 4);
}

#[cfg(windows)]
#[test]
fn a_bare_name_that_finds_a_cmd_shim_is_refused() {
    let outer = tempfile::tempdir().unwrap();
    let outer = outer.path().canonicalize().unwrap();
    let root = outer.join("root");
    let bin = outer.join("bin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("harness"), "#!/bin/sh\n").unwrap();
    std::fs::write(bin.join("harness.cmd"), "@echo off\r\n").unwrap();
    let search = std::env::join_paths([&bin]).unwrap();
    // The shim is what the lookup finds, and a batch file is refused: cmd.exe searches the
    // working directory first for the `node` it calls.
    let error = resolve_executable(Path::new("harness"), Some(&search), &root).unwrap_err();
    assert!(
        matches!(&error, LaunchError::InvalidExecutable { reason, .. } if reason.contains("cmd.exe")),
        "{error}"
    );
}

#[cfg(windows)]
#[test]
fn a_windows_child_does_not_search_its_working_directory_for_programs() {
    let root = tempfile::tempdir().unwrap();
    let spec = spec(HarnessKind::Codex, root.path());
    let env = spec.child_env(Vec::new());
    assert_eq!(env["NoDefaultCurrentDirectoryInExePath"], "1");
}

#[test]
fn the_child_search_path_keeps_only_absolute_entries() {
    let root = tempfile::tempdir().unwrap();
    let spec = spec(HarnessKind::Codex, root.path());
    // A rooted entry without a drive resolves against the current drive on Windows.
    let search = std::env::join_paths(["", "bin", ".", r"\bin", BIN_DIRS[0], BIN_DIRS[1]]).unwrap();
    let env = spec.child_env([(OsString::from("PATH"), search)]);
    assert_eq!(env["PATH"], std::env::join_paths(BIN_DIRS).unwrap());
}

#[test]
fn the_child_search_path_drops_entries_inside_the_root() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    std::fs::create_dir(canonical.join("bin")).unwrap();
    let spec = spec(HarnessKind::Codex, root.path());
    let search = std::env::join_paths([
        root.path().join("bin"),
        canonical.join("bin"),
        root.path().to_owned(),
        PathBuf::from(BIN_DIRS[0]),
    ])
    .unwrap();
    let env = spec.child_env([(OsString::from("PATH"), search)]);
    assert_eq!(env["PATH"], OsString::from(BIN_DIRS[0]));
}

#[test]
fn a_brief_over_a_mebibyte_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let config = config(HarnessKind::Codex, root.path());
    let fits = LaunchRequest::new("x".repeat(crate::session::MAX_PROMPT_BYTES));
    assert!(LaunchSpec::new(&config, &fits).is_ok());
    let over = LaunchRequest::new("x".repeat(crate::session::MAX_PROMPT_BYTES + 1));
    assert!(matches!(
        LaunchSpec::new(&config, &over),
        Err(LaunchError::PromptTooLarge(len)) if len == crate::session::MAX_PROMPT_BYTES + 1
    ));
}

#[test]
fn a_brief_starting_with_a_slash_is_refused_for_every_harness() {
    // Claude Code 2.1.325 ran a stream-json prompt of `/context` as its own command (a
    // synthetic reply, no model turn) instead of handing it to the model.
    let root = tempfile::tempdir().unwrap();
    for kind in [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Acp(AcpFlavor::Gemini),
        HarnessKind::Acp(AcpFlavor::OpenCode),
    ] {
        let config = config(kind, root.path());
        for brief in ["/context", "  \n/clear", "\t/model opus"] {
            assert!(matches!(
                LaunchSpec::new(&config, &LaunchRequest::new(brief)),
                Err(LaunchError::SlashCommandPrompt)
            ));
        }
        assert!(LaunchSpec::new(&config, &LaunchRequest::new("Summarize src/main.rs, not /etc.")).is_ok());
    }
}

#[test]
fn debug_output_names_environment_variables_without_their_values() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::Codex, root.path());
    spec.env
        .insert("OPENAI_API_KEY".to_owned(), "sk-config-secret".to_owned());
    let mut server = McpServerSpec::basemind_serve(MCP_COMMAND);
    server
        .env
        .insert("GITHUB_TOKEN".to_owned(), "ghp-server-secret".to_owned());
    spec.mcp_server = Some(server.clone());
    let plan = crate::CodexAdapter.plan_spec(&spec).unwrap();
    assert_eq!(plan.env["OPENAI_API_KEY"], "sk-config-secret");
    for shown in [format!("{plan:?}"), format!("{server:?}")] {
        assert!(!shown.contains("secret"), "{shown}");
    }
    assert!(format!("{plan:?}").contains("OPENAI_API_KEY"));
    assert!(format!("{server:?}").contains("GITHUB_TOKEN"));
}

#[test]
fn debug_output_leaves_out_the_mcp_servers_arguments() {
    let root = tempfile::tempdir().unwrap();
    let mut server = McpServerSpec::basemind_serve(MCP_COMMAND);
    server.args.push("--token=server-secret".to_owned());
    for kind in [HarnessKind::ClaudeCode, HarnessKind::Codex] {
        let mut spec = spec(kind, root.path());
        spec.mcp_server = Some(server.clone());
        let plan = match kind {
            HarnessKind::ClaudeCode => crate::ClaudeCodeAdapter.plan_spec(&spec),
            _ => crate::CodexAdapter.plan_spec(&spec),
        }
        .unwrap();
        assert!(plan.args.iter().any(|arg| arg.contains("server-secret")), "{kind}");
        let shown = format!("{plan:?}");
        assert!(!shown.contains("server-secret"), "{shown}");
        assert!(
            shown.contains("--permission-mode") || shown.contains("app-server"),
            "{shown}"
        );
    }
    assert!(!format!("{server:?}").contains("server-secret"));
}

#[test]
fn request_facing_errors_do_not_echo_the_root_or_a_whole_value() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(outer.path().join("sibling")).unwrap();
    let root_text = root.canonicalize().unwrap().display().to_string();

    let mut spec = spec(HarnessKind::Codex, &root);
    spec.working_dir = Some(PathBuf::from("../sibling"));
    let error = spec.prepare().unwrap_err();
    assert!(matches!(error, LaunchError::WorkingDirOutsideRoot { .. }));
    assert!(!error.to_string().contains(&root_text), "{error}");

    // Whether a path outside the root exists, or a path inside it is a directory, is not
    // told apart: the errors read the same.
    let outside = error.to_string();
    std::fs::write(root.join("file"), "").unwrap();
    for dir in ["missing", "../no-such-sibling", "file"] {
        spec.working_dir = Some(PathBuf::from(dir));
        let error = spec.prepare().unwrap_err();
        assert!(
            matches!(error, LaunchError::WorkingDirOutsideRoot { .. }),
            "{dir}: {error}"
        );
        assert_eq!(error.to_string(), outside, "{dir}");
    }

    let config = config(HarnessKind::Codex, &root);
    let mut request = LaunchRequest::new("brief");
    request.working_dir = Some(root.join("a".repeat(200)));
    let error = LaunchSpec::new(&config, &request).err().unwrap();
    assert!(matches!(error, LaunchError::WorkingDirNotRelative(_)), "{error}");
    assert!(!error.to_string().contains("aaaa"), "{error}");

    std::fs::create_dir(root.join(".codex")).unwrap();
    spec.working_dir = None;
    let error = spec.prepare().unwrap_err();
    assert!(matches!(error, LaunchError::ProjectConfig { .. }), "{error}");
    assert!(!error.to_string().contains(&root_text), "{error}");
    std::fs::remove_dir(root.join(".codex")).unwrap();

    spec.working_dir = None;
    spec.model = Some(format!("{}\"", "m".repeat(1000)));
    let LaunchError::ArgumentNotAllowed { argument, .. } = spec.prepare().unwrap_err() else {
        panic!("the model was not refused");
    };
    assert_eq!(argument, format!("{}…", "m".repeat(64)));
}

#[cfg(unix)]
#[test]
fn the_launchers_home_or_the_filesystem_root_is_no_root() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("the test needs HOME"));
    for root in [home.as_path(), Path::new("/")] {
        let result = spec(HarnessKind::Codex, root).prepare();
        assert!(
            matches!(result, Err(LaunchError::RootNotAllowed { .. })),
            "{}: {:?}",
            root.display(),
            result.err()
        );
    }
}
