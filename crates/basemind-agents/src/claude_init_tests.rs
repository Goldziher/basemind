//! What Claude Code reports before and after the brief: the `initialize` answer is checked
//! before the brief goes out, `system/init` after it.

use super::*;

/// The answer Claude Code 2.1.326 gave to `initialize` under the adapter's flags.
const LIVE_INITIALIZE: &str = include_str!("../tests/fixtures/live/claude-2.1.326-initialize-response.json");

fn protocol_defining(defined: &[&str]) -> ClaudeProtocol {
    let defined = defined.iter().map(|name| (*name).to_owned()).collect();
    ClaudeProtocol::new("brief".to_owned(), "default", Vec::new()).defining(defined)
}

/// Feeds `answer` as the reply to `initialize` and returns the result and what was written.
fn answer(protocol: &mut ClaudeProtocol, answer: Value) -> (Result<Step, LaunchError>, Vec<Value>) {
    let (sink, _events) = EventSink::for_test(None, 1);
    let mut out = Vec::new();
    protocol.start(&mut out).unwrap();
    out.clear();
    let result = protocol.frame(answer, &mut out, &sink);
    (result, written(&out))
}

#[test]
fn the_brief_waits_for_an_initialize_answer_that_reports_the_mode_and_only_built_in_commands() {
    let live: Value = serde_json::from_str(LIVE_INITIALIZE).unwrap();
    let (result, written) = answer(&mut protocol_defining(&[]), live.clone());
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["type"], "user");

    let mut user_skill = live.clone();
    user_skill["response"]["response"]["commands"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "gh-stack", "description": "a user skill"}));
    let mut other_mode = live.clone();
    other_mode["response"]["response"]["current_permission_mode"] = "acceptEdits".into();
    let mut project_agent = live;
    project_agent["response"]["response"]["agents"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "probe-agent"}));
    for (frame, expected) in [
        (user_skill, "gh-stack"),
        (other_mode, "acceptEdits"),
        (project_agent, "probe-agent"),
    ] {
        let (result, written) = answer(&mut protocol_defining(&["probe-agent"]), frame);
        let error = result.unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(written.is_empty(), "the brief went out: {written:?}");
    }
}

#[test]
fn an_init_frame_listing_a_plugin_or_project_skill_or_agent_fails() {
    let run = |field: &str, name: &str| {
        let (sink, _events) = EventSink::for_test(None, 1);
        let mut protocol = protocol_defining(&["probe-skill", "probe-agent"]);
        let mut frame = json!({"type": "system", "subtype": "init", "permissionMode": "default",
            "mcp_servers": [], "tools": [], "agents": ["Explore"], "skills": ["debug"]});
        frame[field].as_array_mut().unwrap().push(name.into());
        protocol.frame(frame, &mut Vec::new(), &sink)
    };
    assert!(run("skills", "verify").is_ok());
    for (field, name) in [
        ("skills", "example:example-skill"),
        ("skills", "probe-skill"),
        ("agents", "probe-agent"),
    ] {
        let error = run(field, name).unwrap_err();
        assert!(error.to_string().contains(name), "{field} {name}: {error}");
    }
}

#[test]
fn the_names_a_project_or_the_user_defines_are_collected() {
    let outer = tempfile::tempdir().unwrap();
    let home = outer.path().join("home");
    let cwd = outer.path().join("repo/crates/app");
    for dir in [
        home.join(".claude/skills/user-skill"),
        outer.path().join("repo/.claude/agents"),
        outer.path().join("repo/.claude/skills/probe-skill"),
        cwd.join(".claude/commands"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(outer.path().join("repo/.claude/agents/probe-agent.md"), "").unwrap();
    std::fs::write(cwd.join(".claude/commands/deploy.md"), "").unwrap();
    let mut names = defined_names(&cwd, Some(&home));
    names.sort();
    assert_eq!(names, ["deploy", "probe-agent", "probe-skill", "user-skill"]);
}
