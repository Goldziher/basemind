//! Sessions against stand-in harnesses: small shell scripts that replay recorded frames. No
//! real harness is started and nothing here touches the network.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use basemind_agents::{
    AcpFlavor, AgentIdentity, ApprovalRequest, ApprovalSource, ApprovalVerdict, Event, FinishStatus, Harness,
    HarnessKind, LaunchConfig, LaunchError, LaunchRequest, PermissionMode, Registry, Session, UnverifiedModeOptIn,
};

/// Claude Code's handshake, up to the brief and the init frame of a supervised session.
const HANDSHAKE: &str = r#"read -r init
printf '%s\n' '{"type":"control_response","response":{"request_id":"basemind-init"}}'
read -r prompt
printf '%s\n' '{"type":"system","subtype":"init","permissionMode":"default","mcp_servers":[],"tools":[]}'
"#;

/// Writes an executable `/bin/sh` script that stands in for a harness. It ignores its
/// arguments and speaks whatever `body` prints.
fn stand_in(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("stand-in");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A config and a request, as the engine would pair them.
struct Launch {
    config: LaunchConfig,
    request: LaunchRequest,
}

fn spec(kind: HarnessKind, dir: &Path, body: &str) -> Launch {
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let mut config = LaunchConfig::new(
        Harness::new(kind).with_executable(stand_in(dir, body)),
        root,
        AgentIdentity::new("launched-agent").unwrap(),
    );
    // A stand-in that is never told to finish shows up as a wrong status, not as a hang.
    config.limits.wall_time = Duration::from_secs(20);
    Launch {
        config,
        request: LaunchRequest::new("Summarize the README."),
    }
}

fn launch(spec: &Launch) -> Session {
    Registry::with_defaults().launch(&spec.config, &spec.request).unwrap()
}

fn alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .output()
        .unwrap()
        .status
        .success()
}

/// Shell that starts a long-lived grandchild and records its pid once it is seen running.
fn spawn_grandchild(pidfile: &Path) -> String {
    format!(
        "sleep 300 &\nkill -0 $! && echo \"$! alive\" > '{}'\n",
        pidfile.display()
    )
}

/// The pid the stand-in recorded for its grandchild. The stand-in vouches that it ran: by
/// the time a test looks, a session that ends quickly has already killed it.
fn grandchild(pidfile: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let recorded = std::fs::read_to_string(pidfile).unwrap_or_default();
        if let Some(pid) = recorded.trim().strip_suffix(" alive") {
            return pid.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "the stand-in never saw its grandchild running"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A command prefix that runs the rest of the line in a session of its own, so it leaves the
/// stand-in's process group the way a daemonizing helper does. Resolved to an absolute path here,
/// since the stand-in runs in the launcher's cleaned environment. `set -m` is no substitute: dash
/// (Linux's `/bin/sh`) turns job control off without a terminal and keeps the job in the group.
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

fn assert_gone(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} survived the session");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_scripted_claude_turn_runs_to_completion_in_a_clean_environment() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"env > "{dir}/env"
pwd > "{dir}/cwd"
read -r init
printf '%s\n' '{{"type":"control_response","response":{{"request_id":"basemind-init"}}}}'
read -r prompt
printf '%s\n' "$prompt" > "{dir}/prompt"
echo 'Loading plugins...'
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s-1","permissionMode":"default","mcp_servers":[],"tools":[]}}'
printf '%s\n' '{{"type":"stream_event","event":{{"type":"message_start"}}}}'
printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"It is a code map."}}}}}}'
printf '%s\n' '{{"type":"result","result":"It is a code map."}}'
cat > /dev/null
"#,
        dir = dir.path().display()
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    spec.config.env.insert("EXTRA_API_KEY".to_owned(), "yes".to_owned());
    assert!(
        std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
        "the launcher has no variable outside the allow-list, so the check below proves nothing"
    );

    let session = launch(&spec);
    let events: Vec<_> = session.events().collect();
    assert_eq!(
        events,
        [
            Event::HarnessSession("s-1".to_owned()),
            Event::Text("It is a code map.".to_owned()),
            Event::TurnCompleted { turn: 1 },
            Event::Finished(FinishStatus::Completed),
        ]
    );

    let prompt: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("prompt")).unwrap()).unwrap();
    assert_eq!(prompt["message"]["content"], "Summarize the README.");

    let cwd = std::fs::read_to_string(dir.path().join("cwd")).unwrap();
    assert_eq!(Path::new(cwd.trim()), dir.path().join("root").canonicalize().unwrap());

    let env = std::fs::read_to_string(dir.path().join("env")).unwrap();
    let names: Vec<&str> = env
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name)
        .collect();
    assert!(
        env.lines().any(|line| line == "BASEMIND_AGENT_ID=launched-agent"),
        "{env}"
    );
    assert!(env.lines().any(|line| line == "BASEMIND_HOST=claude"), "{env}");
    assert!(env.lines().any(|line| line == "EXTRA_API_KEY=yes"), "{env}");
    assert!(
        env.lines().any(|line| line == "GIT_CONFIG_KEY_0=core.fsmonitor"),
        "{env}"
    );
    let leaked: Vec<_> = names
        .iter()
        .filter(|name| {
            // The shell itself sets these three.
            !matches!(**name, "PWD" | "SHLVL" | "_" | "OLDPWD")
                && !basemind_agents::ENV_ALLOWLIST.contains(name)
                && !matches!(**name, "BASEMIND_AGENT_ID" | "BASEMIND_HOST" | "EXTRA_API_KEY")
                && !name.starts_with("GIT_CONFIG_")
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "variables outside the allow-list reached the child: {leaked:?}"
    );
}

#[test]
fn a_scripted_codex_turn_runs_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"read -r initialize
printf '%s\n' '{{"id":1,"result":{{}}}}'
read -r initialized
read -r thread
printf '%s\n' "$thread" > "{dir}/thread"
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-1"}}}}}}'
read -r turn
printf '%s\n' '{{"method":"turn/started","params":{{"threadId":"thread-1","turn":{{"id":"turn-1"}}}}}}'
printf '%s\n' '{{"method":"item/agentMessage/delta","params":{{"itemId":"m1","delta":"It builds."}}}}'
printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"thread-1","turn":{{"id":"turn-1","status":"completed"}}}}}}'
cat > /dev/null
"#,
        dir = dir.path().display()
    );
    let session = launch(&spec(HarnessKind::Codex, dir.path(), &body));
    let events: Vec<_> = session.events().collect();
    assert_eq!(
        events,
        [
            Event::HarnessSession("thread-1".to_owned()),
            Event::Text("It builds.".to_owned()),
            Event::TurnCompleted { turn: 1 },
            Event::Finished(FinishStatus::Completed),
        ]
    );
    let thread: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("thread")).unwrap()).unwrap();
    assert_eq!(thread["params"]["sandbox"], "read-only");
    assert_eq!(
        Path::new(thread["params"]["cwd"].as_str().unwrap()),
        dir.path().join("root").canonicalize().unwrap()
    );
}

#[test]
fn the_wall_time_limit_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild");
    let body = format!("{}{HANDSHAKE}cat > /dev/null\n", spawn_grandchild(&pidfile));
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    spec.config.limits.wall_time = Duration::from_secs(3);

    let started = Instant::now();
    let session = launch(&spec);
    let leader = session.pid().to_string();
    let grandchild = grandchild(&pidfile);
    assert_eq!(session.wait(), FinishStatus::WallTimeExceeded);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the limit did not end the session"
    );
    assert_gone(&grandchild);
    assert_gone(&leader);
}

#[test]
fn the_wall_time_limit_ends_the_session_even_when_a_descendant_keeps_the_pipes() {
    let Some(own_session) = own_session_prefix() else {
        eprintln!("skipped: neither setsid nor perl is on PATH to move a descendant out of the group");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("escaped");
    // The background job runs in a session of its own, as a daemonizing helper would. It
    // inherits the stand-in's stdout and outlives the group.
    let body = format!(
        "{own_session} sleep 300 &\nkill -0 $! && echo \"$! alive\" > '{}'\n{HANDSHAKE}cat > /dev/null\n",
        pidfile.display()
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    spec.config.limits.wall_time = Duration::from_secs(3);
    let started = Instant::now();
    let session = launch(&spec);
    let escaped = grandchild(&pidfile);
    let leader = session.pid().to_string();
    // Polled against a deadline: a session that waits for the escaped process never ends.
    let deadline = started + Duration::from_secs(10);
    let status = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match session.recv_timeout(left) {
            Ok(Event::Finished(status)) => break Some(status),
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    assert_eq!(status, Some(FinishStatus::WallTimeExceeded));
    assert_gone(&leader);
    assert!(
        alive(&escaped),
        "the stand-in's job was in the group after all, so this proved nothing"
    );
    Command::new("/bin/kill").arg(&escaped).status().unwrap();
}

#[test]
fn a_turn_beyond_the_limit_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild");
    let body = format!(
        r#"{}read -r initialize
printf '%s\n' '{{"id":1,"result":{{}}}}'
read -r initialized
read -r thread
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-1"}}}}}}'
read -r turn
printf '%s\n' '{{"method":"turn/started","params":{{"threadId":"thread-1","turn":{{"id":"turn-1"}}}}}}'
printf '%s\n' '{{"method":"turn/started","params":{{"threadId":"thread-1","turn":{{"id":"turn-2"}}}}}}'
cat > /dev/null
"#,
        spawn_grandchild(&pidfile)
    );
    let session = launch(&spec(HarnessKind::Codex, dir.path(), &body));
    let grandchild = grandchild(&pidfile);
    let events: Vec<_> = session.events().collect();
    assert_eq!(
        events,
        [
            Event::HarnessSession("thread-1".to_owned()),
            Event::Finished(FinishStatus::TurnLimitExceeded),
        ]
    );
    assert_gone(&grandchild);
}

#[test]
fn the_group_is_ended_once_the_allowed_turns_have_run() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild");
    // After its one turn the stand-in ignores both the closed stdin and SIGTERM.
    let body = format!(
        r#"trap '' TERM
{}{HANDSHAKE}printf '%s\n' '{{"type":"result","result":"Done."}}'
while :; do sleep 0.05; done
"#,
        spawn_grandchild(&pidfile)
    );
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let grandchild = grandchild(&pidfile);
    let leader = session.pid().to_string();
    assert_eq!(session.wait(), FinishStatus::Completed);
    assert_gone(&grandchild);
    assert_gone(&leader);
}

#[test]
fn an_approval_request_is_denied_on_the_wire_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{"command":"rm -rf /"}}}}}}'
read -r answer
printf '%s\n' "$answer" > "{dir}/answer"
printf '%s\n' '{{"type":"result","result":"Refused."}}'
cat > /dev/null
"#,
        dir = dir.path().display()
    );
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    assert_eq!(session.wait(), FinishStatus::Completed);
    let answer: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("answer")).unwrap()).unwrap();
    assert_eq!(answer["response"]["request_id"], "req-1");
    assert_eq!(answer["response"]["response"]["behavior"], "deny");
}

#[test]
fn a_request_still_pending_when_the_harness_fails_is_reported_withdrawn() {
    let claude = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{}}}}}}'
sleep 0.3
"#
    );
    let codex = r#"read -r initialize
printf '%s\n' '{"id":1,"result":{}}'
read -r initialized
read -r thread
printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1"}}}'
read -r turn
printf '%s\n' '{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}'
printf '%s\n' '{"id":"r-1","method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"ls"}}'
sleep 0.3
printf '%s\n' '{"method":"error","params":{"error":{"message":"stream lost"}}}'
cat > /dev/null
"#;
    let acp = r#"answer() { read -r line; id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$1"; }
answer '{"protocolVersion":1}'
answer '{"sessionId":"s-1"}'
read -r prompt
printf '%s\n' '{"jsonrpc":"2.0","id":"p-1","method":"session/request_permission","params":{"sessionId":"s-1","toolCall":{"toolCallId":"t-1"},"options":[{"optionId":"a","name":"Allow","kind":"allow_once"}]}}'
sleep 0.3
"#;
    for (kind, body, id) in [
        (HarnessKind::ClaudeCode, claude.as_str(), "claude-1"),
        (HarnessKind::Codex, codex, "codex-1"),
        (HarnessKind::Acp(AcpFlavor::Generic), acp, "acp-1"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec(kind, dir.path(), body);
        spec.config.unverified_mode = Some(UnverifiedModeOptIn {
            harness: kind,
            mode: PermissionMode::Supervised,
        });
        spec.config.approval_policy = Some(Arc::new(|_: &ApprovalRequest| ApprovalVerdict::Ask));
        let events: Vec<_> = launch(&spec).events().collect();
        let withdrawn = events.iter().position(|event| {
            *event
                == Event::ApprovalResolved {
                    id: id.to_owned(),
                    allowed: false,
                    source: ApprovalSource::Withdrawn,
                }
        });
        let finished = events.iter().position(|event| matches!(event, Event::Finished(_)));
        assert_eq!(
            events.last(),
            Some(&Event::Finished(FinishStatus::Failed)),
            "{events:?}"
        );
        assert!(withdrawn.is_some_and(|at| Some(at) < finished), "{kind}: {events:?}");
    }
}

#[test]
fn git_run_by_the_harness_does_not_start_the_repositorys_fsmonitor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
        assert!(status.status.success(), "git {args:?}: {status:?}");
    };
    git(&["init", "-q"]);
    let marker = dir.path().join("fsmonitor-ran");
    let hook = dir.path().join("fsmonitor-hook");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    git(&["config", "core.fsmonitor", hook.to_str().unwrap()]);
    std::fs::write(root.join("untracked"), "").unwrap();
    // The control: git run outside the launch starts the hook.
    git(&["status", "--porcelain"]);
    assert!(marker.exists(), "the fixture's fsmonitor hook never ran");
    std::fs::remove_file(&marker).unwrap();

    let body = format!(
        "{HANDSHAKE}git status --porcelain > \"{dir}/status\" 2>&1\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\ncat > /dev/null\n",
        dir = dir.path().display()
    );
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    assert_eq!(session.wait(), FinishStatus::Completed);
    let status = std::fs::read_to_string(dir.path().join("status")).unwrap();
    assert!(status.contains("untracked"), "git did not run: {status}");
    assert!(!marker.exists(), "the harness's git ran the repository's fsmonitor");
}

#[test]
fn a_policy_that_panics_fails_the_session_and_still_ends_the_harness() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{}}}}}}'
cat > /dev/null
"#
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    let policy = |_: &ApprovalRequest| -> ApprovalVerdict { panic!("the policy is broken") };
    spec.config.approval_policy = Some(Arc::new(policy));
    let session = launch(&spec);
    let leader = session.pid().to_string();
    let events: Vec<_> = session.events().collect();
    let [.., Event::Error(error), Event::Finished(FinishStatus::Failed)] = events.as_slice() else {
        panic!("unexpected events {events:?}");
    };
    assert!(error.contains("panicked"), "{error}");
    assert_gone(&leader);
}

#[test]
fn a_truncated_frame_fails_the_session_with_the_stderr_tail() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!("{HANDSHAKE}echo 'fatal: out of memory' >&2\nprintf '%s' '{{\"type\":\"assistant\",\"mess'\n");
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let events: Vec<_> = session.events().collect();
    let [
        Event::Diagnostics(diagnostics),
        Event::Error(error),
        Event::Finished(FinishStatus::Failed),
    ] = events.as_slice()
    else {
        panic!("unexpected events {events:?}");
    };
    assert!(diagnostics.contains("invalid JSON"), "{diagnostics}");
    assert!(diagnostics.contains("fatal: out of memory"), "{diagnostics}");
    // The stderr tail is the launcher's: the error a model may be shown leaves it out.
    assert!(error.contains("invalid JSON"), "{error}");
    assert!(!error.contains("out of memory"), "{error}");
}

#[test]
fn the_error_of_a_failed_session_is_one_bounded_line() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x".repeat(20_000);
    let body = format!(
        r#"read -r initialize
printf '%s\n' '{{"id":1,"result":{{}}}}'
read -r initialized
read -r thread
printf '%s\n' '{{"id":2,"error":{{"message":"no thread\nfor /home/user/secret\u001b[2J {long}"}}}}'
cat > /dev/null
"#
    );
    let events: Vec<_> = launch(&spec(HarnessKind::Codex, dir.path(), &body)).events().collect();
    let [
        Event::Diagnostics(diagnostics),
        Event::Error(error),
        Event::Finished(FinishStatus::Failed),
    ] = events.as_slice()
    else {
        panic!("unexpected events {events:?}");
    };
    assert!(diagnostics.contains(&long), "the launcher lost the full text");
    assert!(
        error.starts_with("harness protocol error: Codex reported: no thread"),
        "{error:.200}"
    );
    assert!(error.len() <= 512, "{} bytes", error.len());
    assert!(!error.chars().any(char::is_control), "{error:.200}");
}

#[test]
fn a_frame_over_sixteen_mebibytes_fails_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!("{HANDSHAKE}head -c 17000000 /dev/zero | tr '\\0' a\ncat > /dev/null\n");
    let started = Instant::now();
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let events: Vec<_> = session.events().collect();
    let [.., Event::Error(error), Event::Finished(FinishStatus::Failed)] = events.as_slice() else {
        panic!(
            "unexpected events {:?}",
            events.iter().map(|e| format!("{e:.80?}")).collect::<Vec<_>>()
        );
    };
    assert!(error.contains("frame longer than 16777216 bytes"), "{error:.200}");
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[test]
fn a_caller_that_reads_late_finds_a_bounded_queue_with_the_text_intact() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"stream_event","event":{{"type":"message_start"}}}}'
i=0
while [ $i -lt 5000 ]; do
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"x"}}}}}}'
  i=$((i+1))
done
printf '%s\n' '{{"type":"result","result":"done"}}'
printf 'printed\n' > "{dir}/printed"
cat > /dev/null
"#,
        dir = dir.path().display()
    );
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let printed = dir.path().join("printed");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !printed.exists() {
        assert!(Instant::now() < deadline, "the stand-in never finished printing");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The adapter has read every frame; give it a moment to queue them.
    std::thread::sleep(Duration::from_millis(500));
    let events: Vec<_> = session.events().collect();
    assert!(events.len() <= 1024 + 2, "{} events were queued", events.len());
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            Event::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "x".repeat(5000));
    assert_eq!(events.last(), Some(&Event::Finished(FinishStatus::Completed)));
}

#[test]
fn a_harness_that_exits_mid_turn_fails_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), HANDSHAKE));
    let events: Vec<_> = session.events().collect();
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::Diagnostics(_),
                Event::Error(_),
                Event::Finished(FinishStatus::Failed)
            ]
        ),
        "{events:?}"
    );
}

#[test]
fn stop_interrupts_the_harness_and_ends_its_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild");
    let body = format!(
        "{}{HANDSHAKE}cat > '{}/after-prompt'\n",
        spawn_grandchild(&pidfile),
        dir.path().display()
    );
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let grandchild = grandchild(&pidfile);
    // The stand-in reads the prompt before it starts recording, so wait for that file.
    let recorded = dir.path().join("after-prompt");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !recorded.exists() {
        assert!(Instant::now() < deadline, "the stand-in never got past the handshake");
        std::thread::sleep(Duration::from_millis(10));
    }
    session.stop();
    assert!(matches!(
        session.send_follow_up("x".repeat(1024 * 1024 + 1)),
        Err(LaunchError::PromptTooLarge(_))
    ));
    assert!(matches!(
        session.send_follow_up(" /compact"),
        Err(LaunchError::SlashCommandPrompt)
    ));
    assert!(matches!(session.send_follow_up("more"), Err(LaunchError::TurnLimit(1))));
    assert_eq!(session.wait(), FinishStatus::Stopped);
    assert_gone(&grandchild);
    let after = std::fs::read_to_string(&recorded).unwrap();
    assert!(
        after.contains("\"subtype\":\"interrupt\""),
        "no interrupt was sent: {after:?}"
    );
}

#[test]
fn the_group_dies_with_a_launcher_that_is_killed() {
    const HELPER: &str = "BASEMIND_AGENTS_DYING_LAUNCHER";
    if let Some(dir) = std::env::var_os(HELPER) {
        let dir = PathBuf::from(dir);
        let pidfile = dir.join("grandchild");
        // Ignores the end of its input, as a harness may.
        let body = format!("{}{HANDSHAKE}while :; do sleep 1; done\n", spawn_grandchild(&pidfile));
        let session = launch(&spec(HarnessKind::ClaudeCode, &dir, &body));
        std::fs::write(dir.join("leader"), session.pid().to_string()).unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut launcher = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_group_dies_with_a_launcher_that_is_killed",
            "--nocapture",
        ])
        .env(HELPER, dir.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let grandchild = grandchild(&dir.path().join("grandchild"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let leader = loop {
        if let Ok(leader) = std::fs::read_to_string(dir.path().join("leader")) {
            break leader;
        }
        assert!(Instant::now() < deadline, "the launcher never started the session");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(alive(&leader) && alive(&grandchild));
    launcher.kill().unwrap();
    launcher.wait().unwrap();
    assert_gone(&grandchild);
    assert_gone(&leader);
}

#[test]
fn dropping_the_session_ends_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild");
    let body = format!("{}{HANDSHAKE}cat > /dev/null\n", spawn_grandchild(&pidfile));
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let grandchild = grandchild(&pidfile);
    let leader = session.pid().to_string();
    let started = Instant::now();
    drop(session);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the drop waited for the wall-time limit"
    );
    assert_gone(&grandchild);
    assert_gone(&leader);
}

#[test]
fn follow_ups_run_as_further_turns_and_stop_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"result","result":"First."}}'
read -r second
printf '%s\n' "$second" > "{dir}/second"
printf '%s\n' '{{"type":"stream_event","event":{{"type":"message_start"}}}}'
printf '%s\n' '{{"type":"result","result":"Second."}}'
cat > /dev/null
"#,
        dir = dir.path().display()
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    spec.config.limits.max_turns = 2;
    let session = launch(&spec);
    session.send_follow_up("Now fix it.").unwrap();
    assert!(matches!(
        session.send_follow_up("And again."),
        Err(LaunchError::TurnLimit(2))
    ));
    let events: Vec<_> = session.events().collect();
    assert_eq!(
        events,
        [
            Event::Text("First.".to_owned()),
            Event::TurnCompleted { turn: 1 },
            Event::Text("Second.".to_owned()),
            Event::TurnCompleted { turn: 2 },
            Event::Finished(FinishStatus::Completed),
        ]
    );
    let second: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("second")).unwrap()).unwrap();
    assert_eq!(second["message"]["content"], "Now fix it.");
}

#[test]
fn launch_errors_are_typed() {
    let dir = tempfile::tempdir().unwrap();
    let mut missing = spec(HarnessKind::ClaudeCode, dir.path(), "");
    missing.config.harness.executable = dir.path().join("no-such-harness");
    assert!(matches!(
        Registry::with_defaults().launch(&missing.config, &missing.request),
        Err(LaunchError::Path { .. })
    ));
    let not_executable = dir.path().join("not-executable");
    std::fs::write(&not_executable, "").unwrap();
    missing.config.harness.executable = not_executable;
    assert!(matches!(
        Registry::with_defaults().launch(&missing.config, &missing.request),
        Err(LaunchError::Spawn { .. })
    ));
    missing.config.harness.executable = dir.path().join("root");
    assert!(matches!(
        Registry::with_defaults().launch(&missing.config, &missing.request),
        Err(LaunchError::InvalidExecutable { .. })
    ));
    let known = spec(HarnessKind::Codex, dir.path(), "");
    assert!(matches!(
        Registry::new().launch(&known.config, &known.request),
        Err(LaunchError::UnknownHarness("codex"))
    ));
    assert!(Registry::with_defaults().adapter(HarnessKind::Codex).is_some());
}

#[test]
fn dropping_a_session_does_not_wait_on_a_harness_that_stopped_reading() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("escaped");
    // Answers the handshake without reading anything, so the brief's turn/start fills the
    // pipe. The escaped job keeps both pipes open after the group is killed.
    let body = format!(
        r#"set -m
sleep 300 &
kill -0 $! && echo "$! alive" > '{}'
printf '%s\n' '{{"id":1,"result":{{}}}}'
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-1"}}}}}}'
while :; do sleep 1; done
"#,
        pidfile.display()
    );
    let mut spec = spec(HarnessKind::Codex, dir.path(), &body);
    // Larger than any pipe buffer, so the write cannot complete.
    spec.request.brief = "x".repeat(1024 * 1024);
    let session = launch(&spec);
    let escaped = grandchild(&pidfile);
    let leader = session.pid().to_string();
    // Let the adapter reach the write that cannot complete.
    std::thread::sleep(Duration::from_millis(500));
    let (dropped, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(session);
        let _ = dropped.send(());
    });
    let returned = done.recv_timeout(Duration::from_secs(10)).is_ok();
    Command::new("/bin/kill").arg(&escaped).status().unwrap();
    assert!(returned, "dropping the session blocked");
    assert_gone(&leader);
}

#[test]
fn a_stopped_harness_gets_only_the_stop_grace() {
    let dir = tempfile::tempdir().unwrap();
    // Ignores the interrupt, the closed stdin and SIGTERM alike.
    let body = format!(
        "trap '' TERM\n{HANDSHAKE}touch '{}/ready'\nwhile :; do sleep 0.05; done\n",
        dir.path().display()
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    spec.config.limits.stop_grace = Duration::from_millis(200);
    let session = launch(&spec);
    let ready = dir.path().join("ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the stand-in never got past the handshake");
        std::thread::sleep(Duration::from_millis(10));
    }
    let leader = session.pid().to_string();
    let stopped = Instant::now();
    session.stop();
    assert_eq!(session.wait(), FinishStatus::Stopped);
    let took = stopped.elapsed();
    assert!(
        took < Duration::from_millis(1500),
        "the stop took {took:?} with a 200 ms grace"
    );
    assert_gone(&leader);
}

#[test]
fn dropping_a_session_that_is_winding_down_kills_the_harness_at_once() {
    let dir = tempfile::tempdir().unwrap();
    // After its one turn the stand-in ignores the closed stdin and SIGTERM: retiring it
    // takes the full 3.5 s of grace unless the drop cuts that short.
    let body = format!(
        "trap '' TERM\n{HANDSHAKE}printf '%s\\n' '{{\"type\":\"result\",\"result\":\"Done.\"}}'\nwhile :; do sleep 0.05; done\n"
    );
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let leader = session.pid().to_string();
    loop {
        match session.recv_timeout(Duration::from_secs(10)) {
            Ok(Event::TurnCompleted { .. }) => break,
            Ok(_) => {}
            Err(error) => panic!("no turn completed: {error:?}"),
        }
    }
    let dropped = Instant::now();
    drop(session);
    let deadline = dropped + Duration::from_millis(1500);
    while alive(&leader) {
        assert!(Instant::now() < deadline, "the harness outlived the drop by 1.5 s");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_acp_agent_that_floods_requests_and_never_reads_fails_the_session() {
    let dir = tempfile::tempdir().unwrap();
    // Every request is answered, and the answers pile up in front of a stdin nobody reads.
    let body = r#"i=0
while [ $i -lt 200000 ]; do
  i=$((i+1))
  printf '{"jsonrpc":"2.0","id":%d,"method":"x/flood","params":{}}\n' $i
done
while :; do sleep 1; done
"#;
    let mut spec = spec(HarnessKind::Acp(AcpFlavor::Generic), dir.path(), body);
    spec.config.unverified_mode = Some(UnverifiedModeOptIn {
        harness: HarnessKind::Acp(AcpFlavor::Generic),
        mode: PermissionMode::Supervised,
    });
    let started = Instant::now();
    let session = launch(&spec);
    let deadline = started + Duration::from_secs(15);
    let mut events = Vec::new();
    while let Ok(event) = session.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        events.push(event);
    }
    let [.., Event::Error(error), Event::Finished(FinishStatus::Failed)] = events.as_slice() else {
        panic!("the session did not fail within 15 s: {events:?}");
    };
    assert!(error.contains("unread"), "{error}");
}

#[test]
fn a_harness_that_stopped_reading_is_not_given_the_exit_grace() {
    let dir = tempfile::tempdir().unwrap();
    // Never reads the brief, which is larger than the pipe, so stdin cannot reach end of
    // file; the harness would only exit on a signal.
    let body = r#"read -r init
printf '%s\n' '{"type":"control_response","response":{"request_id":"basemind-init"}}'
sleep 0.3
printf '%s\n' '{"type":"system","subtype":"init","permissionMode":"default","mcp_servers":[],"tools":[]}'
printf '%s\n' '{"type":"result","result":"Done."}'
while :; do sleep 0.05; done
"#;
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), body);
    spec.request.brief = "x".repeat(1024 * 1024);
    let session = launch(&spec);
    loop {
        match session.recv_timeout(Duration::from_secs(10)) {
            Ok(Event::TurnCompleted { .. }) => break,
            Ok(_) => {}
            Err(error) => panic!("no turn completed: {error:?}"),
        }
    }
    let done = Instant::now();
    assert_eq!(session.wait(), FinishStatus::Completed);
    let took = done.elapsed();
    assert!(took < Duration::from_millis(1200), "retiring took {took:?}");
}

#[test]
fn a_refused_init_frame_lets_the_harness_finish_on_the_end_of_its_input() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("saved");
    // Reports a mode nobody asked for, then saves something once its input ends; a signal
    // would cut the save off.
    let body = format!(
        r#"read -r init
printf '%s\n' '{{"type":"control_response","response":{{"request_id":"basemind-init"}}}}'
read -r prompt
printf '%s\n' '{{"type":"system","subtype":"init","permissionMode":"bypassPermissions","mcp_servers":[],"tools":[]}}'
trap '' TERM
cat > /dev/null
echo saved > '{}'
"#,
        marker.display()
    );
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), &body));
    let events: Vec<Event> = session.events().collect();
    assert_eq!(events.last(), Some(&Event::Finished(FinishStatus::Failed)));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Error(error) if error.contains("permission mode"))),
        "{events:?}"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "saved");
}
