use serde_json::json;

use super::*;

const TOOL: &str = "mcp__plugin_basemind_basemind__agent_post";

#[test]
fn a_call_without_as_gets_the_main_sessions_name() {
    let event = json!({
        "session_id": "abc",
        "tool_name": TOOL,
        "tool_input": {"subject": "s", "body": "b"},
    });
    let answer = pre_tool_use(&event, Injection::Plain).expect("an answer");
    let input = &answer["hookSpecificOutput"]["updatedInput"];
    let expected = hook_sub_name(MAIN_SESSION, "abc").expect("name");
    assert_eq!(input["as"], expected.as_str());
    assert!(expected.as_str().starts_with("session-"));
    assert_eq!(input["subject"], "s", "the rest of the input is kept");
    assert_eq!(answer["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert!(answer["hookSpecificOutput"].get("permissionDecision").is_none());
}

#[test]
fn a_subagents_call_is_named_after_the_subagent() {
    let event = json!({
        "session_id": "abc",
        "agent_id": "agent-1",
        "agent_type": "Explore",
        "tool_name": TOOL,
        "tool_input": {"body": "b", "thread": "t-aaaaaa"},
    });
    let name =
        pre_tool_use(&event, Injection::Plain).expect("an answer")["hookSpecificOutput"]["updatedInput"]["as"].clone();
    let expected = hook_sub_name("Explore", "agent-1").expect("name");
    assert_eq!(name, expected.as_str());
    assert!(expected.as_str().starts_with("explore-"));
}

#[test]
fn an_explicit_as_and_other_tools_are_left_alone() {
    let given = json!({"session_id": "abc", "tool_name": TOOL, "tool_input": {"as": "lead", "body": "b"}});
    assert_eq!(pre_tool_use(&given, Injection::Plain), None);
    let other = json!({"session_id": "abc", "tool_name": "Bash", "tool_input": {"command": "ls"}});
    assert_eq!(pre_tool_use(&other, Injection::Plain), None);
}

#[test]
fn two_sessions_get_two_names_and_one_session_keeps_its_own() {
    let name = |session: &str| event_name(&json!({ "session_id": session }));
    assert_ne!(name("one"), name("two"));
    assert_eq!(name("one"), name("one"));
}

#[test]
fn an_unreadable_event_prints_nothing() {
    let mut out = Vec::new();
    run_for(
        HookEvent::PreToolUse,
        Some(Injection::Plain),
        &mut "not json".as_bytes(),
        &mut out,
    )
    .expect("no failure");
    assert!(out.is_empty());
    let mut out = Vec::new();
    run_for(
        HookEvent::Name,
        Some(Injection::Plain),
        &mut r#"{"session_id":"abc"}"#.as_bytes(),
        &mut out,
    )
    .expect("name");
    assert_eq!(
        String::from_utf8(out).expect("utf-8").trim(),
        event_name(&json!({"session_id": "abc"})).expect("name")
    );
}

#[test]
fn a_non_string_as_is_left_for_the_tool_to_refuse() {
    for given in [json!(5), json!({"name": "x"}), json!(["x"])] {
        let event = json!({"session_id": "abc", "tool_name": TOOL, "tool_input": {"as": given, "body": "b"}});
        assert_eq!(pre_tool_use(&event, Injection::Plain), None, "{given}");
    }
    for empty in [json!(null), json!("")] {
        let event = json!({"session_id": "abc", "tool_name": TOOL, "tool_input": {"as": empty, "body": "b"}});
        assert!(pre_tool_use(&event, Injection::Plain).is_some(), "{empty}");
    }
}

#[test]
fn codex_gets_the_name_beside_an_allow_and_claude_without_one() {
    let event = json!({"session_id": "abc", "tool_name": "mcp__basemind__agent_inbox", "tool_input": {}});
    let codex = pre_tool_use(&event, Injection::WithAllow).expect("an answer");
    assert_eq!(codex["hookSpecificOutput"]["permissionDecision"], "allow");
    assert!(codex["hookSpecificOutput"]["updatedInput"]["as"].is_string());
    let claude = pre_tool_use(&event, Injection::Plain).expect("an answer");
    assert!(claude["hookSpecificOutput"].get("permissionDecision").is_none());
}

#[test]
fn only_hosts_that_take_a_rewritten_input_get_a_name() {
    assert_eq!(Injection::for_host(Some("claude")), Some(Injection::Plain));
    assert_eq!(Injection::for_host(Some("Codex")), Some(Injection::WithAllow));
    for host in [None, Some("cursor"), Some("gemini"), Some("copilot"), Some("")] {
        assert_eq!(Injection::for_host(host), None, "{host:?}");
        let mut out = Vec::new();
        run_for(
            HookEvent::Name,
            Injection::for_host(host),
            &mut r#"{"session_id":"abc"}"#.as_bytes(),
            &mut out,
        )
        .expect("name");
        assert!(
            out.is_empty(),
            "{host:?} names nothing: its tool calls act as the principal"
        );
    }
}
