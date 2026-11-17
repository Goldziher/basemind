//! The supervisor against stand-in harnesses: `/bin/sh` scripts that speak Claude Code's
//! handshake. No real harness is started. A flavor other than Claude Code is driven through
//! [`As`], which runs the Claude Code adapter over that flavor's config, so the supervisor's
//! per-flavor behaviour (OpenCode's detached server) is exercised without an ACP stand-in.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::*;
use crate::audit::{FileAuditSink, MemoryAuditSink};
use crate::claude::ClaudeCodeAdapter;
use crate::harness::{AcpFlavor, Harness};
use crate::peer::Peer;
use crate::policy::{Budget, FlavorPolicy, PolicySpec};
use crate::registry::Adapter;
use crate::session::CommandPlan;
use crate::spec::{ApprovalVerdict, PermissionMode};
use crate::test_support::always;

const OPENCODE: HarnessKind = HarnessKind::Acp(AcpFlavor::OpenCode);
const SECRET: &str = "sk-ant-test-secret-value-0123456789";
const BRIEF_MARKER: &str = "brief-marker-do-not-log";

/// Claude Code's handshake, up to the brief and the init frame of a supervised session.
const HANDSHAKE: &str = r#"read -r init
printf '%s\n' '{"type":"control_response","response":{"request_id":"basemind-init"}}'
read -r prompt
printf '%s\n' '{"type":"system","subtype":"init","permissionMode":"default","mcp_servers":[],"tools":[]}'
"#;
const RESULT: &str = r#"printf '%s\n' '{"type":"result","result":"Done."}'
"#;

/// Runs the Claude Code adapter for harnesses of another kind.
struct As(HarnessKind);

impl Adapter for As {
    fn supports(&self, kind: HarnessKind) -> bool {
        kind == self.0
    }

    fn plan(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<CommandPlan, LaunchError> {
        ClaudeCodeAdapter.plan(&as_claude(config), request)
    }

    fn launch(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<Session, LaunchError> {
        ClaudeCodeAdapter.launch(&as_claude(config), request)
    }
}

fn as_claude(config: &LaunchConfig) -> LaunchConfig {
    let mut config = config.clone();
    config.harness.kind = HarnessKind::ClaudeCode;
    config
}

/// A scratch directory holding the root, the stand-ins and what they write.
struct Scratch {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(path.join("root")).unwrap();
        Self { _dir: dir, path }
    }

    /// A config for `kind` whose harness is a stand-in running `body`.
    fn config(&self, kind: HarnessKind, body: &str) -> LaunchConfig {
        use std::os::unix::fs::PermissionsExt;
        let script = self.path.join(format!("{}-stand-in", kind.id()));
        std::fs::write(&script, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = LaunchConfig::new(
            Harness::new(kind).with_executable(script),
            self.path.join("root"),
            AgentIdentity::new("template-agent").unwrap(),
        );
        config.limits.wall_time = Duration::from_secs(20);
        config.limits.stop_grace = Duration::from_millis(300);
        config.env.insert("ANTHROPIC_API_KEY".to_owned(), SECRET.to_owned());
        config
    }

    /// A stand-in body that records its environment, then works until it is stopped.
    fn long(&self) -> String {
        format!(
            "env > '{}/'\"$BASEMIND_AGENT_ID\".env\n{HANDSHAKE}cat > /dev/null\n",
            self.path.display()
        )
    }

    /// The environment the launched agent `agent` recorded.
    fn env_of(&self, agent: &str) -> BTreeMap<String, String> {
        let path = self.path.join(format!("{agent}.env"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains("BASEMIND_LAUNCH_TOKEN=") {
                return text
                    .lines()
                    .filter_map(|line| line.split_once('='))
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect();
            }
            assert!(Instant::now() < deadline, "{agent} never recorded its environment");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn policy(flavors: &[HarnessKind]) -> PolicySpec {
    PolicySpec {
        flavors: flavors.iter().copied().map(FlavorPolicy::supervised).collect(),
        launchers: vec!["lead".to_owned(), "second".to_owned(), "third".to_owned()],
        max_depth: 1,
        max_concurrent: 4,
        max_children: 4,
        wall_time: Duration::from_secs(30),
        budget: None,
    }
}

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(ClaudeCodeAdapter);
    registry.register(As(OPENCODE));
    registry
}

fn start(spec: PolicySpec, configs: Vec<LaunchConfig>) -> (Supervisor, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::default());
    let supervisor = Supervisor::new(LaunchPolicy::new(spec).unwrap(), configs, registry(), audit.clone()).unwrap();
    (supervisor, audit)
}

/// A process outside the supervisor's tree: the test runner's parent.
fn outside() -> Peer {
    Peer::of_pid(std::os::unix::process::parent_id())
}

/// The harness process of session `id`, standing in for a process of that session.
fn inside(supervisor: &Supervisor, id: SessionId) -> Peer {
    Peer::of_pid(supervisor.status(id).unwrap().pid.unwrap())
}

fn lead() -> Caller {
    Caller::principal("lead", outside()).unwrap()
}

fn request() -> LaunchRequest {
    LaunchRequest::new(format!("Review the diff. {BRIEF_MARKER}"))
}

/// The outcomes of the records of `event`, in order.
fn outcomes(audit: &MemoryAuditSink, event: AuditEvent) -> Vec<&'static str> {
    audit
        .records()
        .iter()
        .filter(|record| record.event == event)
        .map(|record| record.outcome)
        .collect()
}

/// The first record of `event`.
fn first(audit: &MemoryAuditSink, event: AuditEvent) -> AuditRecord {
    audit
        .records()
        .into_iter()
        .find(|record| record.event == event)
        .unwrap_or_else(|| panic!("no {} record", event.id()))
}

/// Waits until `audit` holds a record of `event`.
fn await_record(audit: &MemoryAuditSink, event: AuditEvent) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while outcomes(audit, event).is_empty() {
        assert!(Instant::now() < deadline, "no {} record arrived", event.id());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finished(supervisor: &Supervisor, id: SessionId) -> FinishStatus {
    match supervisor.wait(id, Duration::from_secs(15)) {
        Some(SessionState::Finished(status)) => status,
        other => panic!("session {id} did not finish: {other:?}"),
    }
}

fn pid_of(supervisor: &Supervisor, id: SessionId) -> i32 {
    i32::try_from(supervisor.status(id).unwrap().pid.unwrap()).unwrap()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn the_default_policy_refuses_every_launch_and_every_config() {
    let audit = Arc::new(MemoryAuditSink::default());
    let supervisor = Supervisor::new(LaunchPolicy::default(), Vec::new(), registry(), audit.clone()).unwrap();
    let refused = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request());
    assert!(matches!(refused, Err(SupervisorError::Disabled)), "{refused:?}");
    assert_eq!(outcomes(&audit, AuditEvent::Refusal), ["disabled"]);
    assert!(supervisor.list().is_empty());

    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, "");
    let refused = Supervisor::new(LaunchPolicy::default(), vec![config], registry(), audit);
    assert_eq!(refused.err(), Some(PolicyError::ConfigNotAllowed("claude")));
}

#[test]
fn a_config_has_to_match_the_policy_for_its_flavor() {
    let scratch = Scratch::new();
    let policy = || LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap();
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::default());
    let mut read_only = scratch.config(HarnessKind::ClaudeCode, "");
    read_only.permission_mode = PermissionMode::ReadOnly;
    let refused = Supervisor::new(policy(), vec![read_only], registry(), audit.clone());
    assert_eq!(
        refused.err(),
        Some(PolicyError::ConfigMismatch {
            harness: "claude",
            field: "permission_mode"
        })
    );
    let mut stray = scratch.config(HarnessKind::ClaudeCode, "");
    stray.bypass = Some(crate::spec::BypassOptIn {
        harness: HarnessKind::ClaudeCode,
        mode: "bypassPermissions".to_owned(),
    });
    let refused = Supervisor::new(policy(), vec![stray], registry(), audit.clone());
    assert_eq!(
        refused.err(),
        Some(PolicyError::ConfigMismatch {
            harness: "claude",
            field: "bypass"
        })
    );
    let config = scratch.config(HarnessKind::ClaudeCode, "");
    let refused = Supervisor::new(policy(), vec![config.clone(), config], registry(), audit);
    assert_eq!(refused.err(), Some(PolicyError::DuplicateConfig("claude")));
}

#[test]
fn a_config_cannot_widen_permissions_the_policy_did_not_grant() {
    let scratch = Scratch::new();
    let policy = |kinds: &[HarnessKind]| LaunchPolicy::new(policy(kinds)).unwrap();
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::default());
    let mismatch = |harness, field| Some(PolicyError::ConfigMismatch { harness, field });
    // An approval policy that allows everything is a bypass under a supervised flavor.
    let mut allowing = scratch.config(HarnessKind::ClaudeCode, "");
    allowing.approval_policy = always(ApprovalVerdict::Allow);
    let refused = Supervisor::new(
        policy(&[HarnessKind::ClaudeCode]),
        vec![allowing],
        registry(),
        audit.clone(),
    );
    assert_eq!(refused.err(), mismatch("claude", "approval_policy"));
    // An ACP mode nobody verified, consented to in the config alone.
    let gemini = HarnessKind::Acp(AcpFlavor::Gemini);
    let mut unverified = scratch.config(gemini, "");
    unverified.unverified_mode = Some(crate::spec::UnverifiedModeOptIn {
        harness: gemini,
        mode: PermissionMode::Supervised,
    });
    let refused = Supervisor::new(policy(&[gemini]), vec![unverified], registry(), audit.clone());
    assert_eq!(refused.err(), mismatch("gemini", "unverified_mode"));
    // A pre-approved tool, such as the launch tool itself.
    let mut tools = scratch.config(HarnessKind::ClaudeCode, "");
    let mut server = crate::spec::McpServerSpec::basemind_serve("/bin/sh");
    server.allow_tools = vec!["agent_launch".to_owned()];
    tools.mcp_server = Some(server);
    let refused = Supervisor::new(
        policy(&[HarnessKind::ClaudeCode]),
        vec![tools],
        registry(),
        audit.clone(),
    );
    assert_eq!(refused.err(), mismatch("claude", "allow_tools"));
    // A stop grace that would hold a dropped supervisor for a day.
    let mut slow = scratch.config(HarnessKind::ClaudeCode, "");
    slow.limits.stop_grace = Duration::from_secs(24 * 60 * 60);
    let refused = Supervisor::new(policy(&[HarnessKind::ClaudeCode]), vec![slow], registry(), audit);
    assert_eq!(refused.err(), mismatch("claude", "stop_grace"));
}

#[test]
fn only_an_admitted_caller_launches_an_allowed_flavor() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(policy(&[HarnessKind::ClaudeCode, HarnessKind::Codex]), vec![config]);
    let claude = HarnessKind::ClaudeCode;
    let cases = [
        (
            Caller::principal("stranger", outside()).unwrap(),
            claude,
            "caller_not_admitted",
        ),
        (supervisor.operator(outside()).unwrap(), claude, "caller_not_admitted"),
        // A principal with the launched prefix is never a configured launcher.
        (
            Caller::principal("launched-claude-0-00000000", outside()).unwrap(),
            claude,
            "caller_not_admitted",
        ),
        (Caller::launched("0".repeat(64), outside()), claude, "unknown_token"),
        // Allowed by the policy but without a config, and not allowed at all.
        (lead(), HarnessKind::Codex, "flavor_not_allowed"),
        (lead(), HarnessKind::Acp(AcpFlavor::Gemini), "flavor_not_allowed"),
    ];
    for (caller, flavor, code) in &cases {
        let refused = supervisor.launch(caller, *flavor, &request()).unwrap_err();
        assert_eq!(refused.code(), *code, "{caller:?} {flavor}: {refused}");
    }
    assert_eq!(
        outcomes(&audit, AuditEvent::Refusal),
        cases.iter().map(|(_, _, code)| *code).collect::<Vec<_>>()
    );
    let callers: Vec<Option<String>> = audit.records().into_iter().map(|record| record.caller).collect();
    assert_eq!(callers[0].as_deref(), Some("stranger"));
    assert_eq!(callers[1].as_deref(), Some("operator"));
    assert_eq!(callers[3], None, "a token caller was named");
    assert!(supervisor.list().is_empty(), "a refused launch left a session");
    assert!(Caller::principal("has space", outside()).is_err());
    assert!(!format!("{:?}", Caller::launched("secret-token", outside())).contains("secret-token"));
}

#[test]
fn a_launched_agent_carries_its_depth_and_token_and_launches_only_within_max_depth() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (shallow, audit) = start(policy(&[HarnessKind::ClaudeCode]), vec![config.clone()]);
    let parent = shallow.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    assert_eq!(parent.depth, 1);
    assert!(parent.agent.starts_with("launched-claude-0-"), "{}", parent.agent);
    let env = scratch.env_of(&parent.agent);
    assert_eq!(env["BASEMIND_AGENT_ID"], parent.agent);
    assert_eq!(env["BASEMIND_AGENT_DEPTH"], "1");
    let token = env["BASEMIND_LAUNCH_TOKEN"].clone();
    assert_eq!(token.len(), 64);

    let refused = shallow
        .launch(
            &Caller::launched(&token, inside(&shallow, parent.id)),
            HarnessKind::ClaudeCode,
            &request(),
        )
        .unwrap_err();
    assert!(
        matches!(refused, SupervisorError::DepthExceeded { depth: 2, max_depth: 1 }),
        "{refused}"
    );
    assert!(
        refused.to_string().contains("max_depth"),
        "the limit is not named: {refused}"
    );
    let record = audit.records().into_iter().last().unwrap();
    assert_eq!(
        (record.caller.as_deref(), record.caller_depth, record.depth),
        (Some(parent.agent.as_str()), Some(1), Some(2))
    );
    shallow.stop(&lead(), parent.id).unwrap();
    assert_eq!(finished(&shallow, parent.id), FinishStatus::Stopped);
    // The token of a finished session admits nobody.
    let refused = shallow
        .launch(
            &Caller::launched(&token, outside()),
            HarnessKind::ClaudeCode,
            &request(),
        )
        .unwrap_err();
    assert_eq!(refused.code(), "unknown_token");

    let (deep, _) = start(
        PolicySpec {
            max_depth: 2,
            ..policy(&[HarnessKind::ClaudeCode])
        },
        vec![config],
    );
    let parent = deep.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let token = scratch.env_of(&parent.agent)["BASEMIND_LAUNCH_TOKEN"].clone();
    let child = deep
        .launch(
            &Caller::launched(&token, inside(&deep, parent.id)),
            HarnessKind::ClaudeCode,
            &request(),
        )
        .unwrap();
    assert_eq!(child.depth, 2);
    assert_eq!(scratch.env_of(&child.agent)["BASEMIND_AGENT_DEPTH"], "2");
    assert_eq!(deep.status(child.id).unwrap().launcher, parent.agent);
    // The configured launcher did not launch the child: only its parent and the operator stop it.
    assert_eq!(deep.stop(&lead(), child.id).unwrap_err().code(), "not_owner");
    deep.stop(&Caller::launched(&token, inside(&deep, parent.id)), child.id)
        .unwrap();
    assert_eq!(finished(&deep, child.id), FinishStatus::Stopped);
}

#[test]
fn a_peer_inside_a_session_is_that_session_whatever_it_claims() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, _) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let pid = supervisor.status(launched.id).unwrap().pid.unwrap();
    // The session's leader claims to be the configured launcher.
    let refused = supervisor
        .launch(
            &Caller::principal("lead", Peer::of_pid(pid)).unwrap(),
            HarnessKind::ClaudeCode,
            &request(),
        )
        .unwrap_err();
    assert!(
        matches!(refused, SupervisorError::DepthExceeded { depth: 2, .. }),
        "{refused}"
    );
    // This process is in no session's group: its claim stands.
    let own = Caller::principal("lead", Peer::this_process().unwrap()).unwrap();
    let second = supervisor.launch(&own, HarnessKind::ClaudeCode, &request()).unwrap();
    assert_eq!(second.depth, 1);
}

#[test]
fn concurrency_and_fan_out_are_refused_with_the_limit_named() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, _) = start(
        PolicySpec {
            max_concurrent: 2,
            max_children: 1,
            ..policy(&[HarnessKind::ClaudeCode])
        },
        vec![config],
    );
    let claude = HarnessKind::ClaudeCode;
    let first = supervisor.launch(&lead(), claude, &request()).unwrap();
    let refused = supervisor.launch(&lead(), claude, &request()).unwrap_err();
    assert!(
        matches!(refused, SupervisorError::FanOutExceeded { max_children: 1 }),
        "{refused}"
    );
    assert!(refused.to_string().contains("max_children"));
    supervisor
        .launch(&Caller::principal("second", outside()).unwrap(), claude, &request())
        .unwrap();
    let third = Caller::principal("third", outside()).unwrap();
    let refused = supervisor.launch(&third, claude, &request()).unwrap_err();
    assert!(
        matches!(refused, SupervisorError::ConcurrencyExceeded { max_concurrent: 2 }),
        "{refused}"
    );
    assert!(refused.to_string().contains("max_concurrent"));

    supervisor.stop(&lead(), first.id).unwrap();
    assert_eq!(finished(&supervisor, first.id), FinishStatus::Stopped);
    supervisor.launch(&third, claude, &request()).unwrap();
}

#[test]
fn the_budget_caps_launches_and_turns_and_an_adapter_refusal_spends_none() {
    let scratch = Scratch::new();
    let mut config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    config.limits.max_turns = 5;
    let (supervisor, audit) = start(
        PolicySpec {
            budget: Some(Budget {
                max_launches: 1,
                max_turns: 1,
            }),
            ..policy(&[HarnessKind::ClaudeCode])
        },
        vec![config],
    );
    let claude = HarnessKind::ClaudeCode;
    let refused = supervisor
        .launch(&lead(), claude, &LaunchRequest::new("/context"))
        .unwrap_err();
    assert!(
        matches!(refused, SupervisorError::Launch(LaunchError::SlashCommandPrompt)),
        "{refused}"
    );
    supervisor.launch(&lead(), claude, &request()).unwrap();
    let refused = supervisor.launch(&lead(), claude, &request()).unwrap_err();
    assert!(
        matches!(refused, SupervisorError::BudgetExhausted { max_launches: 1 }),
        "{refused}"
    );
    let launches: Vec<AuditRecord> = audit
        .records()
        .into_iter()
        .filter(|record| record.event == AuditEvent::Launch)
        .collect();
    assert_eq!(
        launches.len(),
        2,
        "both admitted launches were recorded before starting"
    );
    assert_eq!(
        launches[1].limits.unwrap().max_turns,
        1,
        "the budget did not cap the turns"
    );
    assert_eq!(
        outcomes(&audit, AuditEvent::Refusal),
        ["slash_command_prompt", "budget_exhausted"]
    );
}

#[test]
fn the_policy_wall_time_ends_a_session_that_runs_too_long() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(
        PolicySpec {
            wall_time: Duration::from_secs(1),
            ..policy(&[HarnessKind::ClaudeCode])
        },
        vec![config],
    );
    let started = Instant::now();
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let pid = pid_of(&supervisor, launched.id);
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::WallTimeExceeded);
    assert!(started.elapsed() < Duration::from_secs(10), "the config's 20 s applied");
    assert!(!alive(pid));
    assert_eq!(
        first(&audit, AuditEvent::Launch).limits.unwrap().wall_time,
        Duration::from_secs(1)
    );
    assert_eq!(outcomes(&audit, AuditEvent::Exit), ["wall_time_exceeded"]);
}

#[test]
fn only_the_launcher_or_the_operator_stops_a_session_and_reaping_forgets_it() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let second = Caller::principal("second", outside()).unwrap();
    assert_eq!(supervisor.stop(&second, launched.id).unwrap_err().code(), "not_owner");
    let stranger = Caller::principal("stranger", outside()).unwrap();
    assert_eq!(
        supervisor.stop(&stranger, launched.id).unwrap_err().code(),
        "caller_not_admitted"
    );
    assert_eq!(
        supervisor.stop(&lead(), SessionId::from_raw(99)).unwrap_err().code(),
        "unknown_session"
    );
    assert_eq!(supervisor.status(launched.id).unwrap().state, SessionState::Running);
    supervisor
        .stop(&supervisor.operator(outside()).unwrap(), launched.id)
        .unwrap();
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Stopped);
    supervisor.stop(&lead(), launched.id).unwrap();
    assert_eq!(
        outcomes(&audit, AuditEvent::Stop),
        [
            "not_owner",
            "caller_not_admitted",
            "unknown_session",
            "requested",
            "already_finished"
        ]
    );
    let reaped = supervisor.reap();
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].state, SessionState::Finished(FinishStatus::Stopped));
    assert!(supervisor.list().is_empty());
    assert!(supervisor.status(launched.id).is_none());
}

#[test]
fn dropping_the_supervisor_stops_every_session() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let pids: Vec<i32> = ["lead", "second"]
        .into_iter()
        .map(|launcher| {
            let caller = Caller::principal(launcher, outside()).unwrap();
            let launched = supervisor.launch(&caller, HarnessKind::ClaudeCode, &request()).unwrap();
            pid_of(&supervisor, launched.id)
        })
        .collect();
    assert!(pids.iter().all(|pid| alive(*pid)));
    let started = Instant::now();
    drop(supervisor);
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the drop took {:?}",
        started.elapsed()
    );
    for pid in pids {
        assert!(!alive(pid), "{pid} outlived the supervisor");
    }
    assert_eq!(outcomes(&audit, AuditEvent::Stop), ["requested", "requested"]);
    assert_eq!(outcomes(&audit, AuditEvent::Exit), ["stopped", "stopped"]);
}

/// Records every event the supervisor reports.
#[derive(Default)]
struct Recorder(Mutex<Vec<(SessionId, Event)>>);

impl SessionObserver for Recorder {
    fn event(&self, session: SessionId, event: &Event) {
        self.0.lock().unwrap().push((session, event.clone()));
    }
}

impl Recorder {
    fn approval(&self) -> Option<String> {
        self.0.lock().unwrap().iter().find_map(|(_, event)| match event {
            Event::ApprovalRequest(request) => Some(request.id.clone()),
            _ => None,
        })
    }
}

#[test]
fn approval_decisions_are_audited_with_who_settled_them() {
    let scratch = Scratch::new();
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"echo {SECRET}"}}}}}}'
cat > /dev/null
"#
    );
    // Without a policy the request is denied by default.
    let (supervisor, audit) = start(
        policy(&[HarnessKind::ClaudeCode]),
        vec![scratch.config(HarnessKind::ClaudeCode, &body)],
    );
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    await_record(&audit, AuditEvent::Approval);
    let approval = first(&audit, AuditEvent::Approval).approval.unwrap();
    assert_eq!(
        (approval.kind, approval.source, approval.allowed),
        ("command", "default_deny", false)
    );
    supervisor.stop(&lead(), launched.id).unwrap();
    drop(supervisor);

    // A policy that asks leaves it to the launcher, whose answer is audited too.
    let mut asking = scratch.config(HarnessKind::ClaudeCode, &body);
    asking.approval_policy = always(ApprovalVerdict::Ask);
    let consenting = PolicySpec {
        flavors: vec![FlavorPolicy {
            approval_policy: true,
            ..FlavorPolicy::supervised(HarnessKind::ClaudeCode)
        }],
        ..policy(&[])
    };
    let (supervisor, audit) = start(consenting, vec![asking]);
    let recorder = Arc::new(Recorder::default());
    let supervisor = supervisor.with_observer(recorder.clone());
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let approval = loop {
        if let Some(approval) = recorder.approval() {
            break approval;
        }
        assert!(Instant::now() < deadline, "the observer saw no approval request");
        std::thread::sleep(Duration::from_millis(20));
    };
    let second = Caller::principal("second", outside()).unwrap();
    assert_eq!(
        supervisor
            .answer(&second, launched.id, &approval, true)
            .unwrap_err()
            .code(),
        "not_owner"
    );
    supervisor.answer(&lead(), launched.id, &approval, false).unwrap();
    await_record(&audit, AuditEvent::Approval);
    assert_eq!(outcomes(&audit, AuditEvent::ApprovalAnswer), ["not_owner", "denied"]);
    let settled = first(&audit, AuditEvent::Approval).approval.unwrap();
    assert_eq!(
        (settled.kind, settled.source, settled.allowed),
        ("command", "caller", false)
    );
    for record in audit.records() {
        assert!(
            !record.to_json_line().contains(SECRET),
            "a command reached the audit log"
        );
    }
}

#[test]
fn the_audit_log_holds_no_brief_environment_value_or_token() {
    let scratch = Scratch::new();
    let log = scratch.path.join("audit.jsonl");
    let audit = Arc::new(FileAuditSink::open(&log).unwrap());
    let supervisor = Supervisor::new(
        LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap(),
        vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
        registry(),
        audit,
    )
    .unwrap();
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let env = scratch.env_of(&launched.agent);
    let token = env["BASEMIND_LAUNCH_TOKEN"].clone();
    assert_eq!(env["ANTHROPIC_API_KEY"], SECRET, "the stand-in did not see the secret");
    let refused = supervisor.launch(
        &Caller::launched(&token, inside(&supervisor, launched.id)),
        HarnessKind::ClaudeCode,
        &request(),
    );
    assert!(refused.is_err());
    supervisor.stop(&lead(), launched.id).unwrap();
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Stopped);
    drop(supervisor);

    let text = std::fs::read_to_string(&log).unwrap();
    let events: Vec<String> = text
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(events, ["launch", "refusal", "stop", "exit"]);
    for needle in [SECRET, BRIEF_MARKER, token.as_str(), "ANTHROPIC_API_KEY", "PATH"] {
        assert!(!text.contains(needle), "{needle:?} reached the audit log");
    }
    let launch: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(launch["caller"], "lead");
    assert_eq!(launch["agent"], launched.agent);
    assert_eq!(launch["flavor"], "claude");
    assert_eq!(launch["depth"], 1);
    assert_eq!(launch["root"], scratch.path.join("root").to_str().unwrap());
    assert_eq!(launch["limits"]["max_depth"], 1);
}

/// A command prefix that runs the rest of the line in a session of its own, as OpenCode's
/// server leaves the harness's group. `None` where neither `setsid` nor `perl` is installed.
fn own_session_prefix() -> Option<String> {
    let on_path = |name: &str| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    };
    if let Some(setsid) = on_path("setsid") {
        return Some(format!("'{}'", setsid.display()));
    }
    on_path("perl").map(|perl| format!("'{}' -MPOSIX -e 'POSIX::setsid() or die; exec @ARGV'", perl.display()))
}

/// The pid a stand-in wrote to `path`.
fn recorded_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(pid) = std::fs::read_to_string(path).unwrap_or_default().trim().parse() {
            return pid;
        }
        assert!(Instant::now() < deadline, "the stand-in never recorded its server");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_opencode_server_that_left_the_group_is_killed_once_the_session_ends() {
    let Some(own_session) = own_session_prefix() else {
        eprintln!("skipped: neither setsid nor perl is installed");
        return;
    };
    let scratch = Scratch::new();
    let body = |pidfile: &Path| {
        format!(
            "{own_session} sleep 300 &\necho $! > '{}'\n{HANDSHAKE}sleep 1\n{RESULT}cat > /dev/null\n",
            pidfile.display()
        )
    };
    let opencode_pid = scratch.path.join("opencode-server.pid");
    let claude_pid = scratch.path.join("claude-server.pid");
    let (supervisor, audit) = start(
        policy(&[HarnessKind::ClaudeCode, OPENCODE]),
        vec![
            scratch.config(OPENCODE, &body(&opencode_pid)),
            scratch.config(HarnessKind::ClaudeCode, &body(&claude_pid)),
        ],
    );

    let launched = supervisor.launch(&lead(), OPENCODE, &request()).unwrap();
    assert_eq!(launched.caveats, &[Caveat::DetachedServer, Caveat::UserMcpServers]);
    let server = recorded_pid(&opencode_pid);
    assert!(alive(server));
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Completed);
    // The session reports finished once its detached server was killed; init reaps it shortly.
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(server) {
        assert!(
            Instant::now() < deadline,
            "the detached server outlived the OpenCode session"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(first(&audit, AuditEvent::Exit).caveats, ["detached_server"]);

    // A flavor without the caveat is not tracked: its detached process is left running, which
    // shows the kill above was the supervisor's.
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    let server = recorded_pid(&claude_pid);
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Completed);
    let survived = alive(server);
    // SAFETY: the test started this process; it is killed whatever the assertion says.
    unsafe { libc::kill(server, libc::SIGKILL) };
    assert!(survived, "the untracked process died with its group");
}

/// `perl`, which the attack stand-ins use to call `setpgid` and `setsid`. Its absence fails the
/// test unless `BASEMIND_TEST_ALLOW_NO_SETSID` is set, so a missing tool never passes silently.
fn perl() -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("perl"))
        .find(|candidate| candidate.is_file());
    if found.is_none() {
        assert!(
            std::env::var_os("BASEMIND_TEST_ALLOW_NO_SETSID").is_some(),
            "perl is not installed; set BASEMIND_TEST_ALLOW_NO_SETSID=1 to skip the tests that need it"
        );
        eprintln!("skipped: perl is not installed and BASEMIND_TEST_ALLOW_NO_SETSID is set");
    }
    found
}

/// Lead launches A (Claude Code); A launches C (OpenCode), whose stand-in runs `perl <attack>`
/// in the background. Returns the supervisor, both sessions and the attacking process's pid.
/// `attack` is given the scratch directory, where `a.pid` holds A's pid.
struct Lateral {
    scratch: Scratch,
    supervisor: Supervisor,
    a: Launched,
    c: Launched,
    attacker: u32,
    /// The attacker as it started, so it is killed at the end only while it is still that
    /// process.
    started: Peer,
}

impl Drop for Lateral {
    fn drop(&mut self) {
        let pid = self.started.id.pid;
        if crate::procinfo::info(pid).is_some_and(|now| now.start == self.started.id.start) {
            // SAFETY: the test started this process, and its start time was checked just now.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn lateral(attack: impl Fn(&Path) -> String) -> Option<Lateral> {
    let perl = perl()?;
    let scratch = Scratch::new();
    let pidfile = scratch.path.join("attacker.pid");
    let body = format!(
        "{HANDSHAKE}'{perl}' {attack} &\necho $! > '{pidfile}'\ncat > /dev/null\n",
        perl = perl.display(),
        attack = attack(&scratch.path),
        pidfile = pidfile.display(),
    );
    let (supervisor, _) = start(
        PolicySpec {
            max_depth: 3,
            ..policy(&[HarnessKind::ClaudeCode, OPENCODE])
        },
        vec![
            scratch.config(HarnessKind::ClaudeCode, &scratch.long()),
            scratch.config(OPENCODE, &body),
        ],
    );
    let a = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    std::fs::write(scratch.path.join("a.pid"), pid_of(&supervisor, a.id).to_string()).unwrap();
    let token = scratch.env_of(&a.agent)["BASEMIND_LAUNCH_TOKEN"].clone();
    let c = supervisor
        .launch(
            &Caller::launched(&token, inside(&supervisor, a.id)),
            OPENCODE,
            &request(),
        )
        .unwrap();
    let attacker = u32::try_from(recorded_pid(&pidfile)).unwrap();
    Some(Lateral {
        scratch,
        supervisor,
        a,
        c,
        attacker,
        started: Peer::of_pid(attacker),
    })
}

/// What the attacker wrote to `outcome` in the scratch directory, once it has.
fn attack_outcome(scratch: &Scratch) -> String {
    let path = scratch.path.join("outcome");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.is_empty() {
            return text;
        }
        assert!(Instant::now() < deadline, "the attacker never reported");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_child_cannot_join_its_launchers_group_and_act_as_its_launcher() {
    // C's tool process moves into A's process group, then claims to be the configured launcher.
    let Some(attack) = lateral(|dir| {
        format!(
            "-e 'open(my $f, q{{<}}, q{{{dir}/a.pid}}) or die; my $a = <$f>; chomp $a; \
             my $r = setpgrp(0, $a) ? q{{joined}} : q{{refused}}; \
             open(my $o, q{{>}}, q{{{dir}/outcome}}) or die; print $o $r; close $o; sleep 300'",
            dir = dir.display()
        )
    }) else {
        return;
    };
    let outcome = attack_outcome(&attack.scratch);
    let claim = Caller::principal("lead", Peer::of_pid(attack.attacker)).unwrap();
    // Acting as A, C could stop (and answer for) itself; acting as itself it may not.
    let stopped = attack.supervisor.stop(&claim, attack.c.id);
    assert_eq!(outcome, "refused", "C's process joined A's process group");
    assert_eq!(stopped.map_err(|error| error.code()), Err("not_owner"));
    let _ = attack.a;
}

#[test]
fn a_child_that_leaves_for_a_session_of_its_own_is_still_its_session() {
    let Some(attack) = lateral(|dir| {
        format!(
            "-MPOSIX -e 'my $r = POSIX::setsid() > 0 ? q{{left}} : q{{stayed}}; \
             open(my $o, q{{>}}, q{{{dir}/outcome}}) or die; print $o $r; close $o; sleep 300'",
            dir = dir.display()
        )
    }) else {
        return;
    };
    assert_eq!(attack_outcome(&attack.scratch), "left");
    let refused = attack.supervisor.operator(Peer::of_pid(attack.attacker));
    assert_eq!(refused.err().map(|error| error.code()), Some("caller_not_admitted"));
    let claim = Caller::principal("lead", Peer::of_pid(attack.attacker)).unwrap();
    // As the configured launcher it would launch at depth 1 and own A; as C it sits at depth 2.
    let launched = attack
        .supervisor
        .launch(&claim, HarnessKind::ClaudeCode, &request())
        .unwrap();
    assert_eq!(
        launched.depth, 3,
        "the escaped process launched as the configured launcher"
    );
    assert_eq!(
        attack
            .supervisor
            .stop(&claim, attack.a.id)
            .map_err(|error| error.code()),
        Err("not_owner")
    );
}

#[test]
fn a_process_below_the_supervisor_in_no_session_is_no_launcher() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, _) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    // A process the supervisor's process started outside any session, as a leftover of a
    // finished session would be.
    let mut stray = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let claim = Caller::principal("lead", Peer::of_pid(stray.id())).unwrap();
    let refused = supervisor.launch(&claim, HarnessKind::ClaudeCode, &request());
    let _ = stray.kill();
    let _ = stray.wait();
    assert_eq!(
        refused.map(|launched| launched.depth).map_err(|error| error.code()),
        Err("caller_not_admitted")
    );
}

#[test]
fn only_a_peer_outside_the_supervisors_tree_is_made_the_operator() {
    let scratch = Scratch::new();
    let config = scratch.config(HarnessKind::ClaudeCode, &scratch.long());
    let (supervisor, audit) = start(policy(&[HarnessKind::ClaudeCode]), vec![config]);
    let launched = supervisor.launch(&lead(), HarnessKind::ClaudeCode, &request()).unwrap();
    // A launched agent running `basemind agent approve` from its shell: a process of the session.
    let refused = supervisor.operator(inside(&supervisor, launched.id));
    assert_eq!(refused.err().map(|error| error.code()), Some("caller_not_admitted"));
    // A process this process started outside any session.
    let mut stray = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let refused = supervisor.operator(Peer::of_pid(stray.id()));
    let _ = stray.kill();
    let _ = stray.wait();
    assert_eq!(refused.err().map(|error| error.code()), Some("caller_not_admitted"));
    assert_eq!(
        outcomes(&audit, AuditEvent::Refusal),
        ["caller_not_admitted", "caller_not_admitted"]
    );
    // The operator's own process, outside the tree.
    let operator = supervisor.operator(outside()).unwrap();
    supervisor.stop(&operator, launched.id).unwrap();
    assert_eq!(finished(&supervisor, launched.id), FinishStatus::Stopped);
}
