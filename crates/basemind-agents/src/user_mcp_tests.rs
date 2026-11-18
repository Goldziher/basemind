use std::collections::BTreeMap;
use std::ffi::OsString;

use serde_json::{Value, json};

use super::*;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn codex_list_output_yields_every_server_name() {
    // The shape `codex mcp list --json` printed on Codex 0.169, trimmed.
    let out = br#"[{"name":"node_repl","enabled":true,"transport":{"type":"stdio","command":"/x"}},
                   {"name":"codex_app","enabled":false,"disabled_reason":null}]"#;
    assert_eq!(parse_codex_list(out).unwrap(), names(&["node_repl", "codex_app"]));
    assert_eq!(parse_codex_list(b"[]").unwrap(), Vec::<String>::new());
    for bad in [&b"not json"[..], b"{}", br#"[{"enabled":true}]"#, br#"[{"name":7}]"#] {
        assert!(
            matches!(
                parse_codex_list(bad),
                Err(LaunchError::UserMcpServers { harness: "codex", .. })
            ),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    let huge = vec![b' '; 1024 * 1024 + 1];
    assert!(parse_codex_list(&huge).is_err());
}

#[test]
fn codex_switches_off_each_user_server_and_refuses_the_injected_name() {
    assert_eq!(
        codex_disable_args(&names(&["node_repl", "ai-rulez"]), Some("basemind")).unwrap(),
        [
            "-c",
            "mcp_servers.node_repl.enabled=false",
            "-c",
            "mcp_servers.ai-rulez.enabled=false"
        ]
    );
    assert!(codex_disable_args(&[], Some("basemind")).unwrap().is_empty());
    // The user's entry would extend the injected server's settings.
    assert!(matches!(
        codex_disable_args(&names(&["basemind"]), Some("basemind")),
        Err(LaunchError::UserMcpServers { .. })
    ));
    // A name a dotted `-c` path cannot carry is refused, not passed on half-quoted.
    for name in ["a.b", "with space", "", "x=y"] {
        assert!(codex_disable_args(&names(&[name]), None).is_err(), "{name:?}");
    }
}

#[cfg(unix)]
#[test]
fn codex_servers_runs_the_listing_in_the_plans_environment() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let args_file = dir.path().join("args");
    let program = dir.path().join("codex");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\necho \"$@ $PROBE_ENV\" > '{}'\necho '[{{\"name\":\"node_repl\"}}]'\n",
            args_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut env = BTreeMap::new();
    env.insert("PROBE_ENV".to_owned(), OsString::from("from-plan"));
    env.insert("PATH".to_owned(), OsString::from("/usr/bin:/bin"));
    let plan = CommandPlan {
        program: program.clone(),
        args: Vec::new(),
        env,
        cwd: dir.path().to_owned(),
        server_args: 0,
    };
    assert_eq!(codex_servers(&plan).unwrap(), names(&["node_repl"]));
    assert_eq!(
        std::fs::read_to_string(&args_file).unwrap().trim(),
        "-c features.plugins=false -c features.apps=false mcp list --json from-plan"
    );

    std::fs::write(&program, "#!/bin/sh\nexit 3\n").unwrap();
    assert!(matches!(codex_servers(&plan), Err(LaunchError::UserMcpServers { .. })));
}

/// `codex mcp list` from an npm install is a Node shim whose native child holds the output
/// pipe: a listing that hangs, or exits leaving a process that keeps the pipe open, has to be
/// bounded by the timeout and leave nothing of its process group running.
#[cfg(unix)]
#[test]
fn codex_listing_is_bounded_when_a_child_keeps_its_output_open() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("codex");
    let pid_file = dir.path().join("pid");
    let mut env = BTreeMap::new();
    env.insert("PATH".to_owned(), OsString::from("/usr/bin:/bin"));
    let plan = CommandPlan {
        program: program.clone(),
        args: Vec::new(),
        env,
        cwd: dir.path().to_owned(),
        server_args: 0,
    };
    let holder_gone = || {
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // SAFETY: kill with signal 0 only checks that the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        true
    };
    for (script, listed) in [
        // Hangs, with a background child holding the pipe.
        ("sleep 30 &\necho $! > \"$PID_FILE\"\nexec sleep 30\n", false),
        // Prints its list and exits, leaving the background child holding the pipe.
        (
            "sleep 30 &\necho $! > \"$PID_FILE\"\necho '[{\"name\":\"node_repl\"}]'\n",
            true,
        ),
    ] {
        let script = script.replace("$PID_FILE", &pid_file.display().to_string());
        std::fs::write(&program, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let result = codex_servers_within(&plan, std::time::Duration::from_secs(1));
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{script}");
        if listed {
            assert_eq!(result.unwrap(), names(&["node_repl"]));
        } else {
            assert!(matches!(result, Err(LaunchError::UserMcpServers { .. })), "{script}");
        }
        assert!(holder_gone(), "the listing's background child survived: {script}");
    }
}

fn servers(user: &OpenCodeUser) -> Vec<String> {
    user.servers.clone()
}

/// A configuration directory and a working directory outside it, both empty.
fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().canonicalize().unwrap();
    let (global, cwd) = (root.join("config"), root.join("work/repo"));
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (outer, global, cwd)
}

#[test]
fn opencode_reads_server_names_from_both_user_files() {
    let (_outer, dir, cwd) = dirs();
    assert_eq!(opencode_user(&dir, &cwd).unwrap(), OpenCodeUser::default());
    std::fs::write(
        dir.join("opencode.json"),
        r#"{"mcp": {"ai-rulez": {"type": "local", "command": ["x"]}}, "plugin": ["p"]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("opencode.jsonc"),
        "{\n  // a comment with \"quotes\"\n  \"mcp\": {\n    \"remote-one\": {\"type\": \"remote\", \"url\": \"https://a//b\"},\n    /* block */ \"ai-rulez\": {},\n  },\n}\n",
    )
    .unwrap();
    assert_eq!(
        servers(&opencode_user(&dir, &cwd).unwrap()),
        names(&["ai-rulez", "remote-one"])
    );

    std::fs::write(dir.join("opencode.jsonc"), "{\"mcp\": [}").unwrap();
    assert!(matches!(
        opencode_user(&dir, &cwd),
        Err(LaunchError::UserMcpServers {
            harness: "opencode",
            ..
        })
    ));
    std::fs::write(dir.join("opencode.jsonc"), r#"{"mcp": ["a"]}"#).unwrap();
    assert!(opencode_user(&dir, &cwd).is_err());
}

/// OpenCode 2.3.2 also takes its canonical form, `mcp.servers.<name>`, beside the legacy
/// `mcp.<name>`, and reads `mcp.timeout` as a setting and an entry naming only `enabled` as
/// nothing (checked with `opencode debug config`).
#[test]
fn opencode_reads_the_canonical_server_table_and_skips_settings() {
    let (_outer, dir, cwd) = dirs();
    std::fs::write(
        dir.join("opencode.json"),
        r#"{"mcp": {"servers": {"canon": {"type": "local", "command": ["x"]}}, "timeout": 5,
            "legacy": {"type": "remote", "url": "https://a"}, "off": {"enabled": false},
            "timeout2": {"type": "local", "command": ["y"]}}}"#,
    )
    .unwrap();
    assert_eq!(
        servers(&opencode_user(&dir, &cwd).unwrap()),
        names(&["canon", "legacy", "timeout2"])
    );
    // A server whose key is `servers` or `timeout` is still a server.
    std::fs::write(
        dir.join("opencode.json"),
        r#"{"mcp": {"servers": {"type": "local", "command": ["x"]}, "timeout": {"type": "remote", "url": "https://a"}}}"#,
    )
    .unwrap();
    assert_eq!(
        servers(&opencode_user(&dir, &cwd).unwrap()),
        names(&["servers", "timeout"])
    );
}

/// An agent's own permission rules come after every configuration's top-level ones (OpenCode
/// 2.3.2, `opencode debug agents`), so the launch names each of the user's agents: those of the
/// `agent`, `mode` and `agents` keys and those of the Markdown files.
#[test]
fn opencode_reads_the_users_agents_from_keys_and_markdown_files() {
    let (_outer, dir, cwd) = dirs();
    std::fs::write(
        dir.join("opencode.json"),
        r#"{"agent": {"build": {"permission": {"bash": "allow"}}}, "mode": {"m1": {}},
            "agents": {"canon": {"permissions": []}}}"#,
    )
    .unwrap();
    for file in [
        "agent/helper.md",
        "agent/nested/deep.md",
        "agents/two.md",
        "mode/m2.md",
        "agent/notes.txt",
    ] {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\npermission:\n  bash: allow\n---\nHelp.\n").unwrap();
    }
    // `mode` is not read below its top level.
    std::fs::create_dir_all(dir.join("modes/sub")).unwrap();
    std::fs::write(dir.join("modes/sub/skipped.md"), "x").unwrap();
    let mut agents = opencode_user(&dir, &cwd).unwrap().agents;
    agents.sort();
    assert_eq!(
        agents,
        names(&["build", "canon", "helper", "m1", "m2", "nested/deep", "two"])
    );
}

/// OpenCode 2.3.2 loads `opencode.json{,c}` and `.opencode/` from every ancestor of its working
/// directory up to `/`; the project-file check refuses them in every ancestor but the home
/// directory, so the launch reads them there.
#[test]
fn opencode_reads_the_configuration_of_the_working_directorys_ancestors() {
    let (outer, dir, cwd) = dirs();
    let home = outer.path().canonicalize().unwrap().join("work");
    std::fs::write(
        home.join("opencode.json"),
        r#"{"mcp": {"home-srv": {"type": "local", "command": ["x"]}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".opencode/agent")).unwrap();
    std::fs::write(
        home.join(".opencode/opencode.jsonc"),
        r#"{"agent": {"plan": {"permission": {"bash": "allow"}}},}"#,
    )
    .unwrap();
    std::fs::write(home.join(".opencode/agent/local.md"), "x").unwrap();
    let user = opencode_user(&dir, &cwd).unwrap();
    assert_eq!(user.servers, names(&["home-srv"]));
    assert_eq!(user.agents, names(&["plan", "local"]));
    // An installer's `~/.opencode` holding only `bin` adds nothing; a file there is no directory.
    std::fs::remove_dir_all(home.join(".opencode")).unwrap();
    std::fs::write(home.join(".opencode"), "").unwrap();
    assert_eq!(opencode_user(&dir, &cwd).unwrap().agents, Vec::<String>::new());
}

/// OpenCode substitutes `{env:VAR}` and `{file:path}` in a configuration's text before parsing
/// it, so a key holding one is not the name the launch would switch off.
#[test]
fn opencode_refuses_a_name_it_would_substitute() {
    for config in [
        r#"{"mcp": {"{env:SERVER}": {"type": "local", "command": ["x"]}}}"#,
        r#"{"mcp": {"servers": {"a{file:./n}": {"type": "local", "command": ["x"]}}}}"#,
        r#"{"agent": {"{env:AGENT}": {}}}"#,
    ] {
        let (_outer, dir, cwd) = dirs();
        std::fs::write(dir.join("opencode.json"), config).unwrap();
        assert!(
            matches!(
                opencode_user(&dir, &cwd),
                Err(LaunchError::UserMcpServers {
                    harness: "opencode",
                    ..
                })
            ),
            "{config}"
        );
    }
}

#[test]
fn opencode_config_dir_follows_xdg_then_home() {
    let cwd = std::path::Path::new("/work/repo");
    let mut env = BTreeMap::new();
    assert!(matches!(
        opencode_config_dir(&env, cwd),
        Err(LaunchError::UserMcpServers {
            harness: "opencode",
            ..
        })
    ));
    env.insert("HOME".to_owned(), OsString::from("/home/u"));
    assert_eq!(
        opencode_config_dir(&env, cwd).unwrap(),
        std::path::Path::new("/home/u/.config/opencode")
    );
    // An empty variable is unset; a relative one is taken against the working directory.
    env.insert("XDG_CONFIG_HOME".to_owned(), OsString::new());
    assert_eq!(
        opencode_config_dir(&env, cwd).unwrap(),
        std::path::Path::new("/home/u/.config/opencode")
    );
    env.insert("XDG_CONFIG_HOME".to_owned(), OsString::from("relative"));
    assert_eq!(
        opencode_config_dir(&env, cwd).unwrap(),
        std::path::Path::new("/work/repo/relative/opencode")
    );
    env.insert("XDG_CONFIG_HOME".to_owned(), OsString::from("/xdg"));
    assert_eq!(
        opencode_config_dir(&env, cwd).unwrap(),
        std::path::Path::new("/xdg/opencode")
    );
}

#[test]
fn opencode_content_asks_for_everything_but_the_pre_approved_tools() {
    let none = OpenCodeUser::default();
    assert_eq!(
        opencode_content(&[], Some("basemind"), &none).unwrap(),
        r#"{"permission":"ask"}"#
    );
    assert_eq!(
        opencode_content(&names(&["agent_post"]), None, &none).unwrap(),
        r#"{"permission":"ask"}"#
    );
    let user = OpenCodeUser {
        servers: names(&["ai-rulez"]),
        agents: Vec::new(),
    };
    let content = opencode_content(
        &names(&["agent_post", "agent_inbox", "agent_threads"]),
        Some("basemind"),
        &user,
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        parsed,
        json!({
            "permission": {
                "*": "ask",
                "basemind_agent_post": "allow",
                "basemind_agent_inbox": "allow",
                "basemind_agent_threads": "allow",
            },
            "mcp": {"ai-rulez": {"type": "local", "command": ["/usr/bin/true"], "enabled": false}},
        })
    );
    // OpenCode lets the last matching rule win: `*` has to come first in the text.
    let rules: Vec<&String> = parsed["permission"].as_object().unwrap().keys().collect();
    assert_eq!(rules[0], "*");
    assert!(content.find(r#""*":"ask""#).unwrap() < content.find("basemind_agent_").unwrap());

    let clash = OpenCodeUser {
        servers: names(&["basemind"]),
        agents: Vec::new(),
    };
    assert!(matches!(
        opencode_content(&[], Some("basemind"), &clash),
        Err(LaunchError::UserMcpServers { .. })
    ));
}

/// Each of the user's agents is given the launch's rules, which OpenCode appends after the
/// agent's own: a user's `agent.build.permission.bash = "allow"` would otherwise run shell
/// commands unasked in a supervised launch.
#[test]
fn opencode_content_gives_each_user_agent_the_launch_rules() {
    let user = OpenCodeUser {
        servers: Vec::new(),
        agents: names(&["build", "nested/deep"]),
    };
    let parsed: Value =
        serde_json::from_str(&opencode_content(&names(&["agent_post"]), Some("basemind"), &user).unwrap()).unwrap();
    let rules = json!({"*": "ask", "basemind_agent_post": "allow"});
    assert_eq!(
        parsed,
        json!({
            "permission": rules,
            "agent": {"build": {"permission": rules}, "nested/deep": {"permission": rules}},
        })
    );
    let parsed: Value = serde_json::from_str(&opencode_content(&[], None, &user).unwrap()).unwrap();
    assert_eq!(parsed["agent"]["build"], json!({"permission": "ask"}));
}

#[test]
fn jsonc_comments_and_trailing_commas_are_dropped_outside_strings() {
    let text = "{\"a\": \"// not a comment\", /* x */ \"b\": [1, 2,], // tail\n \"c\": \"é,]\",}";
    let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
    assert_eq!(value, json!({"a": "// not a comment", "b": [1, 2], "c": "é,]"}));
    let escaped = r#"{"a": "q\"//x", }"#;
    let value: Value = serde_json::from_str(&strip_jsonc(escaped)).unwrap();
    assert_eq!(value, json!({"a": "q\"//x"}));
}
