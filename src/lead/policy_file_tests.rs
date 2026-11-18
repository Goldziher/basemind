//! The operator's policy file: the only source of the launch policy, and only when this user alone
//! controls it. Every test works in temp directories, never the real user-level path.

use std::os::unix::fs::PermissionsExt as _;

use super::*;

const SECRET: &str = "sk-test-not-a-real-key";

struct Fixture {
    _dir: tempfile::TempDir,
    lead_root: PathBuf,
    worktrees: PathBuf,
    config_dir: PathBuf,
    harness: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let make = |name: &str| {
            let path = base.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        let lead_root = make("lead");
        let worktrees = make("worktrees");
        let config_dir = make("config");
        let harness = base.join("claude");
        std::fs::write(&harness, "#!/bin/sh\n").unwrap();
        Self {
            _dir: dir,
            lead_root,
            worktrees,
            config_dir,
            harness,
        }
    }

    /// A valid enabled policy with `extra` appended to its `[agents.launch]` table and `flavor`
    /// appended to its Claude Code table.
    fn text(&self, extra: &str, flavor: &str) -> String {
        format!(
            "[agents.launch]\nenabled = true\nworktrees_root = \"{}\"\n{extra}\n\
             [agents.launch.harness.claude]\nexecutable = \"{}\"\npass_env = [\"ANTHROPIC_API_KEY\"]\n{flavor}\n",
            self.worktrees.display(),
            self.harness.display()
        )
    }

    fn write(&self, text: &str) -> PathBuf {
        let path = self.config_dir.join("agents.toml");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn load(&self, path: &Path) -> Result<Loaded, PolicyFileError> {
        let env = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| SECRET.to_owned());
        load(
            path,
            &Context {
                lead_root: &self.lead_root,
                launcher: "lead-agent",
                mcp_command: Path::new("/usr/local/bin/basemind"),
                env: &env,
            },
        )
    }

    fn enabled(&self, path: &Path) -> LeadPolicy {
        match self.load(path) {
            Ok(Loaded::Enabled(policy)) => policy,
            other => panic!("not enabled: {other:?}"),
        }
    }
}

#[test]
fn without_a_file_or_with_launching_off_the_policy_is_off() {
    let fixture = Fixture::new();
    let missing = fixture.config_dir.join("agents.toml");
    assert!(matches!(fixture.load(&missing), Ok(Loaded::Absent)));
    let path = fixture.write("[agents.launch]\nenabled = false\n");
    assert!(matches!(fixture.load(&path), Ok(Loaded::Disabled)));
    let path = fixture.write("");
    assert!(matches!(fixture.load(&path), Ok(Loaded::Disabled)));
}

#[test]
fn a_valid_file_launches_at_depth_one_with_no_allows_and_its_credentials_from_the_environment() {
    let fixture = Fixture::new();
    let path = fixture.write(&fixture.text("max_concurrent = 3", "permission_mode = \"accept_edits\""));
    let lead = fixture.enabled(&path);
    assert_eq!(lead.policy.max_depth(), 1);
    assert_eq!(lead.policy.budget().max_allows, 0);
    assert_eq!(lead.policy.max_concurrent(), 3);
    assert_eq!(lead.policy.launchers(), ["lead-agent"]);
    let rule = lead.policy.flavor(HarnessKind::ClaudeCode).unwrap();
    assert!(!rule.launcher_may_allow && !rule.approval_policy);
    assert!(rule.bypass.is_none() && rule.unverified_mode.is_none());
    assert_eq!(rule.permission_mode, PermissionMode::AcceptEdits);
    let config = &lead.configs[0];
    assert_eq!(config.root, fixture.worktrees);
    assert_eq!(config.env.get("ANTHROPIC_API_KEY").map(String::as_str), Some(SECRET));
    assert!(config.approval_policy.is_none());
    let server = config.mcp_server.as_ref().unwrap();
    assert_eq!(server.args, ["serve"], "the injected serve is never a lead");
    // Only the agent comms tools are pre-approved, and the rule consents to exactly them.
    assert_eq!(server.allow_tools, ["agent_post", "agent_inbox", "agent_threads"]);
    assert_eq!(rule.allow_tools, server.allow_tools);
}

/// Each field a lead model must never get from the file makes it fail to build, and the policy
/// stays off.
#[test]
fn every_forbidden_field_refuses_the_file() {
    let fixture = Fixture::new();
    for (extra, flavor) in [
        ("max_depth = 2", ""),
        ("launchers = [\"other\"]", ""),
        ("max_allows = 10", ""),
        ("", "launcher_may_allow = true"),
        ("", "approval_policy = \"allow\""),
        ("", "bypass = \"bypassPermissions\""),
        ("", "unverified_mode = \"supervised\""),
        ("", "allow_tools = [\"code_read\"]"),
        ("", "permission_mode = \"bypass\""),
        ("", "env = { ANTHROPIC_API_KEY = \"sk\" }"),
        ("", "mcp_command = \"/bin/sh\""),
    ] {
        let path = fixture.write(&fixture.text(extra, flavor));
        let refused = fixture.load(&path);
        assert!(
            matches!(refused, Err(PolicyFileError::Toml { .. })),
            "{extra}{flavor}: {refused:?}"
        );
    }
    // A harness the file cannot configure safely (an unverified ACP mode) is not a key either.
    let path = fixture.write(&format!(
        "{}[agents.launch.harness.gemini]\nexecutable = \"/bin/sh\"\n",
        fixture.text("", "")
    ));
    assert!(matches!(fixture.load(&path), Err(PolicyFileError::Toml { .. })));
}

#[test]
fn a_relative_or_bare_executable_is_refused() {
    let fixture = Fixture::new();
    for executable in ["claude", "bin/claude"] {
        let text = fixture
            .text("", "")
            .replace(&fixture.harness.display().to_string(), executable);
        let path = fixture.write(&text);
        let refused = fixture.load(&path);
        assert!(
            matches!(&refused, Err(PolicyFileError::Invalid { reason, .. }) if reason.contains("absolute")),
            "{executable}: {refused:?}"
        );
    }
}

#[test]
fn a_credential_the_lead_does_not_have_is_an_error_not_a_default() {
    let fixture = Fixture::new();
    let text = fixture.text("", "").replace("ANTHROPIC_API_KEY", "OPENAI_API_KEY");
    let path = fixture.write(&text);
    let refused = fixture.load(&path);
    assert!(
        matches!(&refused, Err(PolicyFileError::Invalid { reason, .. }) if reason.contains("OPENAI_API_KEY")),
        "{refused:?}"
    );
}

/// Review 9 condition 10: launched agents never work in the lead's own tree.
#[test]
fn a_worktrees_root_that_is_contains_or_lies_in_the_lead_root_is_refused() {
    let fixture = Fixture::new();
    let inside = fixture.lead_root.join("wt");
    std::fs::create_dir_all(&inside).unwrap();
    let above = fixture.lead_root.parent().unwrap().to_owned();
    for root in [&fixture.lead_root, &above, &inside] {
        let text = fixture
            .text("", "")
            .replace(&fixture.worktrees.display().to_string(), &root.display().to_string());
        let path = fixture.write(&text);
        let refused = fixture.load(&path);
        assert!(
            matches!(&refused, Err(PolicyFileError::Invalid { reason, .. }) if reason.contains("worktrees_root")),
            "{}: {refused:?}",
            root.display()
        );
    }
}

#[test]
fn a_file_others_can_write_or_a_link_is_refused() {
    let fixture = Fixture::new();
    let path = fixture.write(&fixture.text("", ""));
    // The control: the file as written is accepted.
    fixture.enabled(&path);
    for mode in [0o620, 0o602] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let refused = fixture.load(&path);
        assert!(
            matches!(refused, Err(PolicyFileError::Unsafe { .. })),
            "{mode:o}: {refused:?}"
        );
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&fixture.config_dir, std::fs::Permissions::from_mode(0o770)).unwrap();
    let refused = fixture.load(&path);
    assert!(matches!(refused, Err(PolicyFileError::Unsafe { .. })), "{refused:?}");
    std::fs::set_permissions(&fixture.config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    let link = fixture.config_dir.join("linked.toml");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let refused = fixture.load(&link);
    assert!(matches!(refused, Err(PolicyFileError::Unsafe { .. })), "{refused:?}");
    let hard = fixture.config_dir.join("hard.toml");
    std::fs::hard_link(&path, &hard).unwrap();
    let refused = fixture.load(&path);
    assert!(matches!(refused, Err(PolicyFileError::Unsafe { .. })), "{refused:?}");
}

#[test]
fn a_policy_file_inside_the_lead_workspace_is_refused() {
    let fixture = Fixture::new();
    let path = fixture.lead_root.join("agents.toml");
    std::fs::write(&path, fixture.text("", "")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let refused = fixture.load(&path);
    assert!(matches!(refused, Err(PolicyFileError::Unsafe { .. })), "{refused:?}");
}

/// A mode the harness's adapter refuses at every launch is refused when the file loads, so the
/// lead never advertises a launch that cannot run.
#[test]
fn a_permission_mode_the_harness_cannot_launch_in_is_refused_at_load() {
    let fixture = Fixture::new();
    for (harness, mode) in [
        ("codex", "accept_edits"),
        ("opencode", "read_only"),
        ("opencode", "accept_edits"),
    ] {
        let path = fixture.write(&format!(
            "{}[agents.launch.harness.{harness}]\nexecutable = \"/bin/sh\"\npermission_mode = \"{mode}\"\n",
            fixture.text("", "")
        ));
        let refused = fixture.load(&path);
        assert!(
            matches!(&refused, Err(PolicyFileError::Invalid { reason, .. }) if reason.contains("permission_mode")),
            "{harness} {mode}: {refused:?}"
        );
    }
    // The control: every mode that can launch loads.
    for (harness, mode) in [
        ("codex", "supervised"),
        ("codex", "read_only"),
        ("opencode", "supervised"),
        ("claude", "accept_edits"),
    ] {
        let text = if harness == "claude" {
            fixture.text("", &format!("permission_mode = \"{mode}\""))
        } else {
            format!(
                "{}[agents.launch.harness.{harness}]\nexecutable = \"/bin/sh\"\npermission_mode = \"{mode}\"\n",
                fixture.text("", "")
            )
        };
        fixture.enabled(&fixture.write(&text));
    }
}
