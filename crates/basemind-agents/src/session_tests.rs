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
