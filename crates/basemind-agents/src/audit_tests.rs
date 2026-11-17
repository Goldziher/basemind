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
