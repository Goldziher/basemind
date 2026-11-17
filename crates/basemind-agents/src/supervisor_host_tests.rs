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

/// Runs test `name` alone in a fresh process of this test binary with `env`, as `role`, and
/// returns what it wrote to its [`OUT`] file.
fn rerun(scratch: &Scratch, name: &str, role: &str, env: &[(&str, &str)]) -> String {
    let out = scratch.path.join(role);
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env_remove(ENV_LAUNCH_TOKEN)
        .env_remove(ENV_AGENT_DEPTH)
        .env_remove(crate::host::ALLOW_SAME_PROGRAM_ANCESTOR)
        .env(ROLE, role)
        .env(OUT, &out)
        .envs(env.iter().copied())
        // Its stdin a pipe, as a `serve`'s is.
        .stdin(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take();
    assert!(child.wait().unwrap().success(), "the {role} rerun failed");
    drop(stdin);
    std::fs::read_to_string(&out).unwrap_or_else(|_| panic!("the {role} rerun ran no test: {name} matched nothing"))
}

#[test]
fn a_process_inside_a_launched_session_hosts_no_supervisor() {
    const NAME: &str = "supervisor::host_tests::a_process_inside_a_launched_session_hosts_no_supervisor";
    if let Some(out) = std::env::var_os(OUT) {
        std::fs::write(out, host_outcome()).unwrap();
        return;
    }
    let scratch = Scratch::new();
    let rerun = |role: &str, env: &[(&str, &str)]| rerun(&scratch, NAME, role, env);
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

/// Whether another process of this user can open this process's stdin through `/proc`.
#[cfg(target_os = "linux")]
fn stdin_reachable() -> bool {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(": < /proc/{}/fd/0", std::process::id()))
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// What [`registering_in_process_closes_this_processs_stdio_to_the_users_other_processes`]
/// sees in a fresh process: whether its stdin is reachable before and after it registers, and
/// whether it is dumpable then.
#[cfg(target_os = "linux")]
fn seal_outcome() -> String {
    let before = stdin_reachable();
    let scratch = Scratch::new();
    let (supervisor, _) = start(
        policy(&[HarnessKind::ClaudeCode]),
        vec![scratch.config(HarnessKind::ClaudeCode, &scratch.long())],
    );
    let after = stdin_reachable();
    // SAFETY: prctl with PR_GET_DUMPABLE takes plain integers.
    let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    drop(supervisor);
    format!("before={before} after={after} dumpable={dumpable}")
}

/// The lead's tool calls arrive on the hosting process's stdio: a process of the same user that
/// could open `/proc/<pid>/fd/0` could write them in.
#[cfg(target_os = "linux")]
#[test]
fn registering_in_process_closes_this_processs_stdio_to_the_users_other_processes() {
    const NAME: &str =
        "supervisor::host_tests::registering_in_process_closes_this_processs_stdio_to_the_users_other_processes";
    if let Some(out) = std::env::var_os(OUT) {
        std::fs::write(out, seal_outcome()).unwrap();
        return;
    }
    let scratch = Scratch::new();
    // A fresh process: this one is sealed already by whichever test registered first.
    let seen = rerun(
        &scratch,
        NAME,
        "seal",
        &[(crate::host::ALLOW_SAME_PROGRAM_ANCESTOR, "1")],
    );
    assert_eq!(seen, "before=true after=false dumpable=0");
}

/// Which step of [`a_process_below_a_sealed_host_is_nested_whatever_it_is_called`] a rerun plays:
/// `sealed` or `unsealed` for the middle process, `probe` for the one below it.
#[cfg(target_os = "linux")]
const MARKER_ROLE: &str = "BASEMIND_TEST_MARKER_ROLE";
/// Where the probe writes what `nested()` said.
#[cfg(target_os = "linux")]
const MARKER_OUT: &str = "BASEMIND_TEST_MARKER_OUT";
/// The copy of this test binary, under another name, the middle process starts the probe from.
#[cfg(target_os = "linux")]
const MARKER_COPY: &str = "BASEMIND_TEST_MARKER_COPY";

/// A sealed host's executable cannot be read, so a `serve` started below it from a binary of
/// another name would match it by neither file nor name: the marker the seal sets names it.
#[cfg(target_os = "linux")]
#[test]
fn a_process_below_a_sealed_host_is_nested_whatever_it_is_called() {
    const NAME: &str = "supervisor::host_tests::a_process_below_a_sealed_host_is_nested_whatever_it_is_called";
    // The middle process passes MARKER_OUT and MARKER_COPY on to the probe by inheritance.
    let run = |exe: &std::path::Path, role: &str, env: &[(&str, &std::path::Path)]| {
        let status = std::process::Command::new(exe)
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env_remove(ENV_LAUNCH_TOKEN)
            .env_remove(ENV_AGENT_DEPTH)
            .env_remove(crate::host::ALLOW_SAME_PROGRAM_ANCESTOR)
            .env(MARKER_ROLE, role)
            .envs(env.iter().copied())
            .status()
            .unwrap();
        assert!(status.success(), "the {role} rerun failed");
    };
    match std::env::var(MARKER_ROLE).as_deref() {
        Ok("probe") => {
            let out = std::env::var_os(MARKER_OUT).unwrap();
            let seen = match crate::host::nested() {
                Some(crate::host::Nested::Ancestor(pid)) if pid == std::os::unix::process::parent_id() as i32 => {
                    "parent"
                }
                Some(_) => "other",
                None => "none",
            };
            std::fs::write(out, seen).unwrap();
            return;
        }
        Ok(role @ ("sealed" | "unsealed")) => {
            if role == "sealed" {
                // From a thread that is neither the leader nor the test's own: the leader is named.
                assert!(std::thread::spawn(crate::host::seal_host).join().unwrap());
                let pid = std::process::id();
                for comm in ["/proc/self/comm".to_owned(), format!("/proc/self/task/{pid}/comm")] {
                    let named = std::fs::read_to_string(&comm).unwrap();
                    assert_eq!(named.trim_end(), crate::host::HOST_MARKER, "{comm}");
                }
            }
            run(
                std::path::Path::new(&std::env::var_os(MARKER_COPY).unwrap()),
                "probe",
                &[],
            );
            return;
        }
        _ => {}
    }
    let scratch = Scratch::new();
    let copy = scratch.path.join("other-program");
    std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    let seen = |role: &str| {
        let out = scratch.path.join(format!("{role}.out"));
        run(
            &std::env::current_exe().unwrap(),
            role,
            &[(MARKER_OUT, out.as_path()), (MARKER_COPY, copy.as_path())],
        );
        std::fs::read_to_string(out).unwrap()
    };
    // The control: below an unsealed process of another program, nothing matches.
    assert_eq!(seen("unsealed"), "none");
    assert_eq!(seen("sealed"), "parent");
}
