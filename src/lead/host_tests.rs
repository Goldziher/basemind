//! The lead host in this process, against a `/bin/sh` stand-in for Claude Code that reads its
//! input until it is stopped. No real harness is started; the policy file and the audit directory
//! are temp paths, never the user's.

use std::os::unix::fs::PermissionsExt as _;

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    lead_root: PathBuf,
    worktrees: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for sub in ["lead", "worktrees/one", "worktrees/two", "config"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        std::fs::set_permissions(base.join("config"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let stand_in = base.join("claude-stand-in");
        std::fs::write(&stand_in, "#!/bin/sh\ncat > /dev/null\n").unwrap();
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            lead_root: base.join("lead"),
            worktrees: base.join("worktrees"),
            _dir: dir,
            base,
        }
    }

    fn policy(&self) -> PathBuf {
        let path = self.base.join("config/agents.toml");
        std::fs::write(
            &path,
            format!(
                "[agents.launch]\nenabled = true\nworktrees_root = \"{}\"\nwall_time_secs = 60\n\
                 [agents.launch.harness.claude]\nexecutable = \"{}\"\nstop_grace_secs = 1\n",
                self.worktrees.display(),
                self.base.join("claude-stand-in").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn start(&self, policy_path: PathBuf, audit_dir: PathBuf) -> Result<LeadHost, LeadOff> {
        LeadHost::start(
            &self.lead_root,
            Principal::parse("lead-agent").unwrap(),
            None,
            Setup {
                policy_path,
                audit_dir,
                registry: Registry::with_defaults(),
            },
        )
    }
}

fn body(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn without_a_policy_file_the_host_stays_off_and_says_where_the_file_belongs() {
    let fixture = Fixture::new();
    let missing = fixture.base.join("config/agents.toml");
    let off = fixture
        .start(missing.clone(), fixture.base.join("audit"))
        .err()
        .unwrap();
    assert!(off.0.contains("agent launching is off"), "{off}");
    assert!(off.0.contains(&missing.display().to_string()), "{off}");
}

/// Review 9 condition 7: no audit log, no launching.
#[test]
fn an_audit_log_that_cannot_be_opened_keeps_launching_off() {
    let fixture = Fixture::new();
    let blocked = fixture.base.join("audit-is-a-file");
    std::fs::write(&blocked, "").unwrap();
    let off = fixture.start(fixture.policy(), blocked).err().unwrap();
    assert!(off.0.contains("audit log"), "{off}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lead_launches_into_its_own_worktree_and_stops_what_it_launched() {
    let fixture = Fixture::new();
    let host = Arc::new(fixture.start(fixture.policy(), fixture.base.join("audit")).unwrap());
    let call = |name: &'static str, arguments: Value| Arc::clone(&host).call(name, arguments);

    for (arguments, code) in [
        (
            json!({ "harness": "gemini", "brief": "b", "working_dir": "one" }),
            "flavor_not_allowed",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "" }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "." }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "../lead" }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "/tmp" }),
            "working_dir_invalid",
        ),
        (
            json!({ "harness": "claude", "brief": "b", "working_dir": "missing" }),
            "working_dir_invalid",
        ),
    ] {
        let refused = call(AGENT_LAUNCH, arguments.clone()).await;
        assert_eq!(refused["isError"], true, "{arguments}");
        assert_eq!(body(&refused)["error"], code, "{arguments}");
    }
    let unknown = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "b", "working_dir": "one", "executable": "/bin/sh" }),
    )
    .await;
    assert_eq!(unknown["isError"], true);
    assert!(
        body(&unknown)["error"].as_str().unwrap().contains("executable"),
        "{unknown}"
    );

    let launched = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one" }),
    )
    .await;
    assert_eq!(launched["isError"], false, "{launched}");
    let launched = body(&launched);
    let agent = launched["agent"].as_str().unwrap();
    assert!(agent.starts_with("launched-claude-"), "{launched}");
    assert_eq!(launched["depth"], 1);
    assert!(launched.get("thread").is_some(), "{launched}");

    // One worktree, one agent: a second launch into it is refused while the first runs.
    let shared = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one" }),
    )
    .await;
    assert_eq!(body(&shared)["error"], "working_dir_in_use", "{shared}");

    let session = launched["session"].as_u64().unwrap();
    let stopped = call(AGENT_STOP, json!({ "session": session })).await;
    assert_eq!(stopped["isError"], false, "{stopped}");
    let unknown = call(AGENT_STOP, json!({ "session": session + 100 })).await;
    assert_eq!(body(&unknown)["error"], "unknown_session");
    let id = SessionId::from_raw(session);
    let state = host.supervisor.wait(id, std::time::Duration::from_secs(10));
    assert!(matches!(state, Some(SessionState::Finished(_))), "{state:?}");
    // Once it finished, its worktree is free again.
    let again = call(
        AGENT_LAUNCH,
        json!({ "harness": "claude", "brief": "Wait.", "working_dir": "one" }),
    )
    .await;
    assert_eq!(again["isError"], false, "{again}");
}
