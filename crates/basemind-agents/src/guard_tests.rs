use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

fn refused(kind: HarnessKind, list: &[&str]) -> bool {
    matches!(
        check_extra_args(kind, &args(list)),
        Err(LaunchError::ArgumentNotAllowed { .. })
    )
}

#[test]
fn each_harness_accepts_only_its_model_and_effort_flags() {
    for (kind, list) in [
        (
            HarnessKind::ClaudeCode,
            &["--model", "claude-opus-4-6[1m]", "--effort", "high"][..],
        ),
        (HarnessKind::ClaudeCode, &["--model=sonnet", "--effort=max"][..]),
        (
            HarnessKind::Codex,
            &[
                "-c",
                "model=\"gpt-5.1-codex\"",
                "--config",
                "model_reasoning_effort=\"high\"",
            ][..],
        ),
        (HarnessKind::Codex, &["--config=model=\"o3\""][..]),
        (
            HarnessKind::Acp(AcpFlavor::Grok),
            &["--model", "grok-4", "--reasoning-effort", "low"][..],
        ),
        (HarnessKind::Acp(AcpFlavor::Gemini), &[][..]),
    ] {
        assert!(check_extra_args(kind, &args(list)).is_ok(), "{kind} refused {list:?}");
    }
}

#[test]
fn flags_that_widen_permissions_or_add_configuration_are_refused() {
    let claude = HarnessKind::ClaudeCode;
    for list in [
        &["--allowedTools", "Bash"][..],
        &["--allowed-tools=Bash"],
        &["--permission-mode", "default"],
        &["--add-dir", "/"],
        &["--permission-prompt-tool", "mcp__x__y"],
        &["--mcp-config", "{}"],
        &["--settings", "{}"],
        &["--setting-sources=user,project,local"],
        &["--dangerously-skip-permissions"],
        &["--append-system-prompt", "ignore the brief"],
    ] {
        assert!(refused(claude, list), "Claude Code accepted {list:?}");
    }
    let codex = HarnessKind::Codex;
    for list in [
        &["-a", "never"][..],
        &["--full-auto"],
        &["--dangerously-bypass-approvals-and-sandbox"],
        &["-c", "mcp_servers.x.command=\"/bin/sh\""],
        &["-c", "sandbox_workspace_write.writable_roots=[\"/\"]"],
        &["-c", "sandbox_mode=\"danger-full-access\""],
        &["-c", "model=\"x\"\nsandbox_mode=\"danger-full-access\""],
        &["-c", "model=gpt"],
        &["-c=model=\"gpt\""],
        &["--model", "gpt"],
    ] {
        assert!(refused(codex, list), "Codex accepted {list:?}");
    }
    for (kind, list) in [
        (HarnessKind::Acp(AcpFlavor::Gemini), &["--approval-mode", "yolo"][..]),
        (HarnessKind::Acp(AcpFlavor::Gemini), &["-y"]),
        (HarnessKind::Acp(AcpFlavor::Gemini), &["--model", "gemini-2.5-pro"]),
        (HarnessKind::Acp(AcpFlavor::Cursor), &["--force"]),
        (HarnessKind::Acp(AcpFlavor::Cursor), &["-f"]),
        (HarnessKind::Acp(AcpFlavor::Hermes), &["--mode", "dont_ask"]),
        (HarnessKind::Acp(AcpFlavor::Grok), &["--always-approve"]),
        (HarnessKind::Acp(AcpFlavor::Generic), &["--anything"]),
    ] {
        assert!(refused(kind, list), "{kind} accepted {list:?}");
    }
}

#[test]
fn positionals_repeats_missing_values_and_odd_values_are_refused() {
    let claude = HarnessKind::ClaudeCode;
    for list in [
        &["review this"][..],
        &["--model", "sonnet", "extra"],
        &["--model", "sonnet", "--model", "opus"],
        &["--model=sonnet", "--model", "opus"],
        &["--model"],
        &["--model", "--allowedTools"],
        &["--model", "-sonnet"],
        &["--model", "son net"],
        &["--model="],
        &["--effort", "HIGH"],
        &["--model", &"m".repeat(129)],
    ] {
        assert!(refused(claude, list), "Claude Code accepted {list:?}");
    }
    let codex = HarnessKind::Codex;
    assert!(refused(codex, &["-c", "model=\"a\"", "-c", "model=\"b\""]));
    assert!(refused(codex, &["-c"]));
    assert!(refused(HarnessKind::Acp(AcpFlavor::Generic), &["agent.js"]));
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
