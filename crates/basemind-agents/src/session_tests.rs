use super::*;
use crate::harness::HarnessKind;
use crate::test_support::spec;

struct Idle;

impl Driver for Idle {
    fn run(self: Box<Self>, _io: ChildIo, _sink: EventSink) -> Result<(), LaunchError> {
        Ok(())
    }
}

#[test]
fn a_working_directory_swapped_for_a_symlink_after_the_check_is_refused() {
    let outer = tempfile::tempdir().unwrap();
    let outer = outer.path().canonicalize().unwrap();
    let root = outer.join("root");
    let work = root.join("work");
    let elsewhere = outer.join("elsewhere");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut spec = spec(HarnessKind::Codex, &root);
    spec.working_dir = Some("work".into());
    let plan = CommandPlan {
        program: "/bin/sh".into(),
        args: vec!["-c".to_owned(), "true".to_owned()],
        env: BTreeMap::new(),
        cwd: spec.prepare().unwrap().cwd,
        server_args: 0,
    };

    // Between the check and the spawn the directory becomes a link out of the root.
    std::fs::remove_dir(&work).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &work).unwrap();
    let started = start(&plan, &spec, Box::new(Idle), Box::new(|_| Ok(())));
    assert!(
        matches!(started, Err(LaunchError::WorkingDirChanged(ref dir)) if *dir == work),
        "{:?}",
        started.err()
    );

    std::fs::remove_file(&work).unwrap();
    std::fs::create_dir(&work).unwrap();
    let session =
        start(&plan, &spec, Box::new(Idle), Box::new(|_| Ok(()))).expect("the unchanged directory was refused");
    assert_eq!(session.wait(), FinishStatus::Completed);
}

#[test]
fn nothing_is_launched_while_sigchld_is_ignored() {
    const CHILD: &str = "BASEMIND_AGENTS_SIGCHLD_IGNORED_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // SAFETY: this re-executed test process sets its own SIGCHLD disposition.
        unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
        let root = tempfile::tempdir().unwrap();
        let spec = spec(HarnessKind::Codex, root.path());
        let plan = CommandPlan {
            program: "/bin/sh".into(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            env: BTreeMap::new(),
            cwd: spec.prepare().unwrap().cwd,
            server_args: 0,
        };
        let started = start(&plan, &spec, Box::new(Idle), Box::new(|_| Ok(())));
        assert!(
            matches!(started, Err(LaunchError::Unsupported { .. })),
            "{:?}",
            started.err()
        );
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session::tests::nothing_is_launched_while_sigchld_is_ignored",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("1 passed"), "the child ran no test: {stdout}");
}
