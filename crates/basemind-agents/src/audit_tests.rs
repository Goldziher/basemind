use super::*;

fn full_record() -> AuditRecord {
    AuditRecord {
        caller: Some("lead".to_owned()),
        caller_depth: Some(0),
        session: Some(7),
        agent: Some("launched-claude-7-00ff00ff".to_owned()),
        flavor: Some("claude"),
        root: Some(PathBuf::from("/work/repo")),
        depth: Some(1),
        limits: Some(AuditLimits {
            wall_time: Duration::from_secs(90),
            max_turns: 2,
            max_depth: 1,
            max_concurrent: 4,
            max_children: 2,
        }),
        caveats: vec!["detached_server"],
        approval: Some(AuditApproval {
            id: "approval-1".to_owned(),
            kind: "command",
            allowed: false,
            source: "default_deny",
        }),
        ..AuditRecord::new(AuditEvent::Launch, "accepted")
    }
}

#[test]
fn a_record_is_one_json_object_with_every_field_it_carries() {
    let line = full_record().to_json_line();
    assert!(!line.contains('\n'));
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["event"], "launch");
    assert_eq!(value["caller"], "lead");
    assert_eq!(value["caller_depth"], 0);
    assert_eq!(value["session"], 7);
    assert_eq!(value["flavor"], "claude");
    assert_eq!(value["root"], "/work/repo");
    assert_eq!(value["depth"], 1);
    assert_eq!(value["limits"]["wall_time_s"], 90);
    assert_eq!(value["limits"]["max_children"], 2);
    assert_eq!(value["caveats"], json!(["detached_server"]));
    assert_eq!(value["approval"]["source"], "default_deny");
    assert_eq!(value["outcome"], "accepted");
    assert!(value["ts_ms"].as_u64().unwrap() > 0);
}

#[test]
fn absent_fields_are_left_out() {
    let value: Value = serde_json::from_str(&AuditRecord::new(AuditEvent::Refusal, "disabled").to_json_line()).unwrap();
    let mut keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["event", "outcome", "ts_ms"]);
}

#[cfg(unix)]
#[test]
fn the_file_sink_creates_a_private_log_and_appends_one_line_per_record() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let sink = FileAuditSink::open(&path).unwrap();
    assert_eq!(sink.path(), path);
    sink.record(&full_record()).unwrap();
    sink.record(&AuditRecord::new(AuditEvent::Exit, "completed")).unwrap();
    drop(sink);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    // Reopening appends rather than truncating.
    FileAuditSink::open(&path)
        .unwrap()
        .record(&AuditRecord::new(AuditEvent::Stop, "requested"))
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let events: Vec<String> = text
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(events, ["launch", "exit", "stop"]);
}

#[cfg(unix)]
#[test]
fn the_file_sink_refuses_a_log_others_can_read_and_a_link() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let open = dir.path().join("open.jsonl");
    std::fs::write(&open, "").unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
    let refused = FileAuditSink::open(&open).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);

    let target = dir.path().join("target.jsonl");
    let link = dir.path().join("link.jsonl");
    std::fs::write(&target, "").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(FileAuditSink::open(&link).is_err(), "a link was followed");

    assert!(FileAuditSink::open(dir.path()).is_err(), "a directory was accepted");
}

#[cfg(unix)]
#[test]
fn the_file_sink_refuses_a_hard_link_to_another_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // A private file of the user's, such as a credential store, pre-linked at the log's path.
    let target = dir.path().join("authorized_keys");
    std::fs::write(&target, "ssh-ed25519 AAAA\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("audit.jsonl");
    std::fs::hard_link(&target, &link).unwrap();
    let opened = FileAuditSink::open(&link);
    assert_eq!(
        opened.err().map(|error| error.kind()),
        Some(std::io::ErrorKind::PermissionDenied),
        "a hard link was accepted"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "ssh-ed25519 AAAA\n");
}

#[cfg(unix)]
#[test]
fn the_file_sink_refuses_a_fifo_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("audit.jsonl");
    let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo reads the NUL-terminated path it is given.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (done, opened) = std::sync::mpsc::channel();
    std::thread::spawn({
        let fifo = fifo.clone();
        move || {
            let _ = done.send(FileAuditSink::open(&fifo).map(|_| ()));
        }
    });
    let result = opened.recv_timeout(Duration::from_secs(2));
    if result.is_err() {
        // Release the blocked open so the thread ends, then fail.
        let _ = std::fs::File::open(&fifo);
    }
    let result = result.expect("opening a FIFO blocked");
    assert!(result.is_err(), "a FIFO was accepted");
}

#[cfg(unix)]
#[test]
fn a_full_log_drops_records_with_a_marker_and_counts_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let line = AuditRecord::new(AuditEvent::Refusal, "fan_out_exceeded")
        .to_json_line()
        .len() as u64
        + 1;
    // A refusal may use three quarters of the cap: three lines.
    let sink = FileAuditSink::open(&path).unwrap().with_max_bytes(4 * line);
    let written = (0..10)
        .filter(|_| {
            sink.record(&AuditRecord::new(AuditEvent::Refusal, "fan_out_exceeded"))
                .is_ok()
        })
        .count();
    assert_eq!(written, 3);
    assert_eq!(sink.dropped(), 7);
    drop(sink);
    let text = std::fs::read_to_string(&path).unwrap();
    let events: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let names: Vec<&str> = events.iter().map(|event| event["event"].as_str().unwrap()).collect();
    assert_eq!(names, ["refusal", "refusal", "refusal", "audit_full", "audit_dropped"]);
    assert_eq!(events[4]["dropped"], 7);
    assert!(text.len() as u64 <= 4 * line + MARKER_ROOM);
}

#[test]
fn a_full_log_keeps_room_for_launches_exits_and_allows() {
    let dir = tempfile::tempdir().unwrap();
    let sink = FileAuditSink::open(dir.path().join("audit.jsonl"))
        .unwrap()
        .with_max_bytes(8 * 1024);
    let refusal = AuditRecord {
        caller: Some("flooder".to_owned()),
        ..AuditRecord::new(AuditEvent::Refusal, "caller_not_admitted")
    };
    let mut written = 0;
    while sink.record(&refusal).is_ok() {
        written += 1;
        assert!(written < 10_000, "the cap never applied");
    }
    // Refusals filled what they may use; a launch, an allow and an exit still go on the record.
    sink.record(&AuditRecord::new(AuditEvent::Launch, "accepted")).unwrap();
    sink.record(&AuditRecord {
        approval: Some(AuditApproval {
            id: "1".to_owned(),
            kind: "command",
            allowed: true,
            source: "caller",
        }),
        ..AuditRecord::new(AuditEvent::ApprovalAnswer, "allowed")
    })
    .unwrap();
    sink.record(&AuditRecord::new(AuditEvent::Exit, "completed")).unwrap();
    // The stop that stopped a session is kept too; a repeated one and a denial are not.
    sink.record(&AuditRecord::new(AuditEvent::Stop, "requested")).unwrap();
    sink.record(&AuditRecord::new(AuditEvent::Stop, "cascaded")).unwrap();
    assert!(
        sink.record(&AuditRecord::new(AuditEvent::Stop, "already_stopping"))
            .is_err()
    );
    assert!(
        sink.record(&AuditRecord {
            approval: Some(AuditApproval {
                id: "2".to_owned(),
                kind: "command",
                allowed: false,
                source: "policy",
            }),
            ..AuditRecord::new(AuditEvent::Approval, "denied")
        })
        .is_err()
    );
}
