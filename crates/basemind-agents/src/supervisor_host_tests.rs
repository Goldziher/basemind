//! Which processes may host a supervisor: never one inside a launched session, nor one a
//! process of the same program started.

use super::tests::*;
use super::*;
use crate::audit::MemoryAuditSink;
use crate::identity::{ENV_AGENT_DEPTH, ENV_LAUNCH_TOKEN};

/// Which case [`a_process_inside_a_launched_session_hosts_no_supervisor`] plays in a rerun.
const ROLE: &str = "BASEMIND_TEST_HOST_ROLE";
/// Where the rerun writes what it saw, so a rerun that matched no test fails the outer one.
const OUT: &str = "BASEMIND_TEST_HOST_OUT";

/// What this process gets when it builds a launching supervisor and when it binds the operator
/// to a disabled one.
fn host_outcome() -> String {
    let scratch = Scratch::new();
    let built = Supervisor::new(
        LaunchPolicy::new(policy(&[HarnessKind::ClaudeCode])).unwrap(),
        vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
        registry(),
        Arc::new(MemoryAuditSink::default()),
    );
    let audit = Arc::new(MemoryAuditSink::default());
    let disabled = Supervisor::new(LaunchPolicy::default(), Vec::new(), registry(), audit.clone()).unwrap();
    let bound = disabled.register_operator_in_process();
    format!(
        "new={:?} register={:?} recorded={:?}",
        built.err(),
        bound.err().map(|error| error.code()),
        outcomes(&audit, AuditEvent::Registration)
    )
}

#[test]
fn a_process_inside_a_launched_session_hosts_no_supervisor() {
    const NAME: &str = "supervisor::host_tests::a_process_inside_a_launched_session_hosts_no_supervisor";
    if let Some(out) = std::env::var_os(OUT) {
        std::fs::write(out, host_outcome()).unwrap();
        return;
    }
    let scratch = Scratch::new();
    let rerun = |role: &str, env: &[(&str, &str)]| {
        let out = scratch.path.join(role);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env_remove(ENV_LAUNCH_TOKEN)
            .env_remove(ENV_AGENT_DEPTH)
            .env_remove(crate::host::ALLOW_SAME_PROGRAM_ANCESTOR)
            .env(ROLE, role)
            .env(OUT, &out)
            .envs(env.iter().copied());
        assert!(command.status().unwrap().success(), "the {role} rerun failed");
        std::fs::read_to_string(&out).unwrap_or_else(|_| panic!("the {role} rerun ran no test: {NAME} matched nothing"))
    };
    let allowed = "new=None register=None recorded=[\"registered\"]";
    let refused = "new=Some(NestedHost) register=Some(\"nested_host\") recorded=[\"nested_host\"]";
    let skip_ancestry = (crate::host::ALLOW_SAME_PROGRAM_ANCESTOR, "1");
    // The control: with the ancestry check skipped and no session variable, both are allowed.
    assert_eq!(rerun("control", &[skip_ancestry]), allowed);
    // A launched session's variables refuse it, whatever its ancestry.
    assert_eq!(rerun("token", &[skip_ancestry, (ENV_LAUNCH_TOKEN, "a-token")]), refused);
    assert_eq!(rerun("depth", &[skip_ancestry, (ENV_AGENT_DEPTH, "1")]), refused);
    // With no variable at all, as a harness that filters its MCP servers' environment starts
    // it, the parent running the same program refuses it.
    assert_eq!(rerun("ancestor", &[]), refused);
}

#[test]
fn this_test_process_may_host_a_supervisor() {
    // No basemind_agents test binary started this one: what every other test relies on.
    assert_eq!(crate::host::nested(), None);
}

#[test]
fn two_processes_run_the_same_program_by_file_or_by_name() {
    use crate::procinfo::Image;
    let image = |comm: Option<&str>, file: Option<(u64, u64)>| Image {
        comm: comm.map(|comm| comm.as_bytes().to_vec()),
        file,
    };
    let serve = image(Some("basemind"), Some((1, 2)));
    assert!(serve.same_program(&image(Some("basemind"), Some((1, 3)))), "a copy");
    assert!(serve.same_program(&image(Some("bm"), Some((1, 2)))), "a link");
    assert!(serve.same_program(&image(None, Some((1, 2)))));
    assert!(!serve.same_program(&image(Some("claude"), Some((4, 5)))));
    assert!(!serve.same_program(&image(Some("claude"), None)));
    assert!(
        !image(None, None).same_program(&image(None, None)),
        "nothing read matches nothing"
    );
}
