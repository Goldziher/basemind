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

#[test]
fn opencode_reads_server_names_from_both_user_files() {
    let dir = tempfile::tempdir().unwrap();
    assert!(opencode_servers(dir.path()).unwrap().is_empty());
    std::fs::write(
        dir.path().join("opencode.json"),
        r#"{"mcp": {"ai-rulez": {"type": "local", "command": ["x"]}}, "plugin": ["p"]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("opencode.jsonc"),
        "{\n  // a comment with \"quotes\"\n  \"mcp\": {\n    \"remote-one\": {\"type\": \"remote\", \"url\": \"https://a//b\"},\n    /* block */ \"ai-rulez\": {},\n  },\n}\n",
    )
    .unwrap();
    assert_eq!(
        opencode_servers(dir.path()).unwrap(),
        names(&["ai-rulez", "remote-one"])
    );

    std::fs::write(dir.path().join("opencode.jsonc"), "{\"mcp\": [}").unwrap();
    assert!(matches!(
        opencode_servers(dir.path()),
        Err(LaunchError::UserMcpServers {
            harness: "opencode",
            ..
        })
    ));
    std::fs::write(dir.path().join("opencode.jsonc"), r#"{"mcp": ["a"]}"#).unwrap();
    assert!(opencode_servers(dir.path()).is_err());
}

#[test]
fn opencode_config_dir_follows_xdg_then_home() {
    let mut env = BTreeMap::new();
    assert_eq!(opencode_config_dir(&env), None);
    env.insert("HOME".to_owned(), OsString::from("/home/u"));
    assert_eq!(
        opencode_config_dir(&env).unwrap(),
        std::path::Path::new("/home/u/.config/opencode")
    );
    env.insert("XDG_CONFIG_HOME".to_owned(), OsString::from("relative"));
    assert_eq!(
        opencode_config_dir(&env).unwrap(),
        std::path::Path::new("/home/u/.config/opencode")
    );
    env.insert("XDG_CONFIG_HOME".to_owned(), OsString::from("/xdg"));
    assert_eq!(
        opencode_config_dir(&env).unwrap(),
        std::path::Path::new("/xdg/opencode")
    );
}

#[test]
fn opencode_content_asks_for_everything_but_the_pre_approved_tools() {
    assert_eq!(
        opencode_content(&[], Some("basemind"), &[]).unwrap(),
        r#"{"permission":"ask"}"#
    );
    assert_eq!(
        opencode_content(&names(&["agent_post"]), None, &[]).unwrap(),
        r#"{"permission":"ask"}"#
    );
    let content = opencode_content(
        &names(&["agent_post", "agent_inbox", "agent_threads"]),
        Some("basemind"),
        &names(&["ai-rulez"]),
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

    assert!(matches!(
        opencode_content(&[], Some("basemind"), &names(&["basemind"])),
        Err(LaunchError::UserMcpServers { .. })
    ));
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
