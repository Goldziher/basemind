//! Sessions against stand-in harnesses: small shell scripts that replay recorded frames. No
//! real harness is started and nothing here touches the network.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use basemind_agents::{
    AgentIdentity, ApprovalRequest, ApprovalVerdict, Event, FinishStatus, Harness, HarnessKind, LaunchError,
    LaunchSpec, Registry, Session,
};

const HANDSHAKE: &str = r#"read -r init
printf '%s\n' '{"type":"control_response","response":{"request_id":"basemind-init"}}'
read -r prompt
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

fn spec(kind: HarnessKind, dir: &Path, body: &str) -> LaunchSpec {
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let mut spec = LaunchSpec::new(
        Harness::new(kind).with_executable(stand_in(dir, body)),
        root,
        "Summarize the README.",
        AgentIdentity::new("launched-agent").unwrap(),
    );
    // A stand-in that is never told to finish shows up as a wrong status, not as a hang.
    spec.limits.wall_time = Duration::from_secs(20);
    spec
}

fn launch(spec: &LaunchSpec) -> Session {
    Registry::with_defaults().launch(spec).unwrap()
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
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s-1"}}'
printf '%s\n' '{{"type":"stream_event","event":{{"type":"message_start"}}}}'
printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"It is a code map."}}}}}}'
printf '%s\n' '{{"type":"result","result":"It is a code map."}}'
cat > /dev/null
"#,
        dir = dir.path().display()
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    spec.env.insert("EXTRA_API_KEY".to_owned(), "yes".to_owned());
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
    let leaked: Vec<_> = names
        .iter()
        .filter(|name| {
            // The shell itself sets these three.
            !matches!(**name, "PWD" | "SHLVL" | "_" | "OLDPWD")
                && !basemind_agents::ENV_ALLOWLIST.contains(name)
                && !matches!(**name, "BASEMIND_AGENT_ID" | "BASEMIND_HOST" | "EXTRA_API_KEY")
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
printf '%s\n' '{{"method":"turn/started","params":{{"turn":{{"id":"turn-1"}}}}}}'
printf '%s\n' '{{"method":"item/agentMessage/delta","params":{{"itemId":"m1","delta":"It builds."}}}}'
printf '%s\n' '{{"method":"turn/completed","params":{{"turn":{{"id":"turn-1","status":"completed"}}}}}}'
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
    spec.limits.wall_time = Duration::from_secs(3);

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
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("escaped");
    // Job control gives the background job a process group of its own, as a daemonizing
    // helper would have. It inherits the stand-in's stdout and outlives the group.
    let body = format!(
        "set -m\nsleep 300 &\nkill -0 $! && echo \"$! alive\" > '{}'\n{HANDSHAKE}cat > /dev/null\n",
        pidfile.display()
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    spec.limits.wall_time = Duration::from_secs(3);
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
printf '%s\n' '{{"method":"turn/started","params":{{"turn":{{"id":"turn-1"}}}}}}'
printf '%s\n' '{{"method":"turn/started","params":{{"turn":{{"id":"turn-2"}}}}}}'
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
fn a_policy_that_panics_fails_the_session_and_still_ends_the_harness() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"{HANDSHAKE}printf '%s\n' '{{"type":"control_request","request_id":"req-1","request":{{"subtype":"can_use_tool","tool_name":"Bash","input":{{}}}}}}'
cat > /dev/null
"#
    );
    let mut spec = spec(HarnessKind::ClaudeCode, dir.path(), &body);
    let policy = |_: &ApprovalRequest| -> ApprovalVerdict { panic!("the policy is broken") };
    spec.approval_policy = Some(Arc::new(policy));
    let session = launch(&spec);
    let leader = session.pid().to_string();
    let events: Vec<_> = session.events().collect();
    let [Event::Error(error), Event::Finished(FinishStatus::Failed)] = events.as_slice() else {
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
    let [Event::Error(error), Event::Finished(FinishStatus::Failed)] = events.as_slice() else {
        panic!("unexpected events {events:?}");
    };
    assert!(error.contains("invalid JSON"), "{error}");
    assert!(error.contains("fatal: out of memory"), "{error}");
}

#[test]
fn a_harness_that_exits_mid_turn_fails_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let session = launch(&spec(HarnessKind::ClaudeCode, dir.path(), HANDSHAKE));
    let events: Vec<_> = session.events().collect();
    assert!(
        matches!(
            events.as_slice(),
            [Event::Error(_), Event::Finished(FinishStatus::Failed)]
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
    spec.limits.max_turns = 2;
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
    missing.harness.executable = dir.path().join("no-such-harness");
    assert!(matches!(
        Registry::with_defaults().launch(&missing),
        Err(LaunchError::Spawn { .. })
    ));
    let known = spec(HarnessKind::Codex, dir.path(), "");
    assert!(matches!(
        Registry::new().launch(&known),
        Err(LaunchError::UnknownHarness("codex"))
    ));
    assert!(Registry::with_defaults().adapter(HarnessKind::Codex).is_some());
}
