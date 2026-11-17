//! The session modes an ACP agent is launched in, and the switches it reports.

use super::*;

#[test]
fn a_mode_whose_behaviour_is_unverified_needs_an_opt_in_naming_harness_and_mode() {
    let root = tempfile::tempdir().unwrap();
    for flavor in [
        AcpFlavor::Cursor,
        AcpFlavor::Gemini,
        AcpFlavor::Grok,
        AcpFlavor::Hermes,
        AcpFlavor::Generic,
    ] {
        let mut spec = spec(HarnessKind::Acp(flavor), root.path());
        assert!(
            matches!(AcpAdapter.plan_spec(&spec), Err(LaunchError::UnverifiedMode { .. })),
            "{flavor:?} launched supervised without consent"
        );
        for (harness, mode) in [
            (HarnessKind::Codex, PermissionMode::Supervised),
            (HarnessKind::Acp(flavor), PermissionMode::ReadOnly),
        ] {
            spec.unverified_mode = Some(UnverifiedModeOptIn { harness, mode });
            assert!(
                matches!(AcpAdapter.plan_spec(&spec), Err(LaunchError::UnverifiedMode { .. })),
                "consent for {harness} / {mode:?} launched {flavor:?}"
            );
        }
        assert!(AcpAdapter.plan_spec(&acp_spec(flavor, root.path())).is_ok());
    }
    for (flavor, mode) in [
        (AcpFlavor::Grok, PermissionMode::ReadOnly),
        (AcpFlavor::OpenCode, PermissionMode::ReadOnly),
        (AcpFlavor::Hermes, PermissionMode::AcceptEdits),
    ] {
        let mut spec = spec(HarnessKind::Acp(flavor), root.path());
        spec.permission_mode = mode;
        assert!(matches!(
            AcpAdapter.plan_spec(&spec),
            Err(LaunchError::UnverifiedMode { .. })
        ));
        assert!(AcpAdapter.plan_spec(&in_mode(spec, mode)).is_ok());
    }
    // OpenCode is told to ask for everything, so its supervised launch needs no consent.
    for mode in [PermissionMode::Supervised, PermissionMode::ReadOnly] {
        let spec = in_mode(spec(HarnessKind::Acp(AcpFlavor::OpenCode), root.path()), mode);
        let plan = AcpAdapter.plan_spec(&spec).unwrap();
        assert_eq!(
            plan.env.get(OPENCODE_ASK.0).and_then(|value| value.to_str()),
            Some(OPENCODE_ASK.1),
            "{mode:?}"
        );
    }
    let supervised = spec(HarnessKind::Acp(AcpFlavor::OpenCode), root.path());
    assert!(AcpAdapter.plan_spec(&supervised).is_ok());
    // A Cursor launch does not carry it.
    let cursor = AcpAdapter.plan_spec(&acp_spec(AcpFlavor::Cursor, root.path())).unwrap();
    assert!(!cursor.env.contains_key(OPENCODE_ASK.0));
}

#[test]
fn an_agent_that_switches_its_own_mode_fails_the_session() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        let initialize = agent.expect("initialize");
        agent.reply(&initialize, &json!({"protocolVersion": 1}));
        let new_session = agent.expect("session/new");
        agent.reply(
            &new_session,
            &json!({"sessionId": "sess-1", "modes": {"currentModeId": "default", "availableModes": [
                {"id": "default", "name": "Default"}, {"id": "yolo", "name": "YOLO"},
            ]}}),
        );
        agent.expect("session/prompt");
        // Reporting the mode it is in is fine; switching away from it is not.
        agent.update(&json!({"sessionUpdate": "current_mode_update", "currentModeId": "default"}));
        agent.update(&json!({"sessionUpdate": "current_mode_update", "currentModeId": "yolo"}));
        agent.expect("session/cancel");
        while futures::executor::block_on(agent.from_client.next()).is_some() {}
    });
    let error = run.result.unwrap_err();
    assert!(
        error.to_string().contains("switched itself to session mode \"yolo\""),
        "{error}"
    );
}

#[test]
fn a_mode_report_matching_the_mode_set_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    let spec = in_mode(
        spec(HarnessKind::Acp(AcpFlavor::OpenCode), root.path()),
        PermissionMode::ReadOnly,
    );
    let run = converse_with(&spec, Vec::new(), |agent| {
        agent.open();
        let set_mode = agent.expect("session/set_mode");
        assert_eq!(set_mode["params"]["modeId"], "plan");
        agent.update(&json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}));
        agent.reply(&set_mode, &json!({}));
        let prompt = agent.expect("session/prompt");
        agent.update(&json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}));
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
}

/// OpenCode 2.3.0's `mode` config option, as its `session/new` returned it, in mode `current`.
fn opencode_mode_option(current: &str) -> Value {
    json!({"id": "mode", "name": "Session Mode", "category": "mode", "type": "select", "currentValue": current,
        "options": [{"value": "build", "name": "Build"}, {"value": "plan", "name": "Plan"}]})
}

#[test]
fn a_mode_switched_through_a_config_option_fails_the_session() {
    let root = tempfile::tempdir().unwrap();
    let read_only = in_mode(
        spec(HarnessKind::Acp(AcpFlavor::OpenCode), root.path()),
        PermissionMode::ReadOnly,
    );
    let effort = json!({"id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
        "currentValue": "high", "options": [{"value": "high", "name": "High"}]});
    let run = converse_with(&read_only, Vec::new(), move |agent| {
        let initialize = agent.expect("initialize");
        agent.reply(&initialize, &json!({"protocolVersion": 1}));
        let new_session = agent.expect("session/new");
        agent.reply(
            &new_session,
            &json!({"sessionId": "sess-1", "configOptions": [opencode_mode_option("build")]}),
        );
        let set_mode = agent.expect("session/set_mode");
        agent.reply(&set_mode, &json!({}));
        agent.expect("session/prompt");
        // Another option changing, or the mode reported as set, is fine.
        agent.update(&json!({"sessionUpdate": "config_option_update",
            "configOptions": [opencode_mode_option("plan"), effort]}));
        agent.update(&json!({"sessionUpdate": "config_option_update",
            "configOptions": [opencode_mode_option("build")]}));
        agent.expect("session/cancel");
        while futures::executor::block_on(agent.from_client.next()).is_some() {}
    });
    let error = run.result.unwrap_err();
    assert!(
        error.to_string().contains("switched itself to session mode \"build\""),
        "{error}"
    );
}

#[test]
fn a_supervised_opencode_session_runs_in_the_mode_its_config_option_reports() {
    let root = tempfile::tempdir().unwrap();
    let supervised = spec(HarnessKind::Acp(AcpFlavor::OpenCode), root.path());
    let run = converse_with(&supervised, Vec::new(), |agent| {
        let initialize = agent.expect("initialize");
        agent.reply(&initialize, &json!({"protocolVersion": 1}));
        let new_session = agent.expect("session/new");
        agent.reply(
            &new_session,
            &json!({"sessionId": "sess-1", "configOptions": [opencode_mode_option("build")]}),
        );
        let prompt = agent.expect("session/prompt");
        agent.update(&json!({"sessionUpdate": "config_option_update",
            "configOptions": [opencode_mode_option("build")]}));
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
}
