use super::*;
use crate::engine::memory::test_support::{DIM, workspace};
use crate::engine::types::memory::{ImportPolicy, MemoryGetParams, MemoryPutParams};
use crate::index::IndexDb;

fn offline(_: &str) -> anyhow::Result<Vec<f32>> {
    Ok(vec![0.0; usize::from(DIM)])
}

fn put(ws: &Workspace, key: &str, value: &str, tags: &[&str], visibility: Visibility) {
    ws.memory_put(MemoryPutParams {
        key: key.into(),
        value: value.into(),
        tags: Some(tags.iter().map(|t| t.to_string()).collect()),
        embed: false,
        visibility,
    })
    .unwrap();
}

fn export(ws: &Workspace, visibility: Visibility) -> MemoryExportResponse {
    ws.memory_export(MemoryExportParams {
        visibility,
        ..MemoryExportParams::default()
    })
    .unwrap()
}

fn import(ws: &Workspace, content: &str, policy: ImportPolicy) -> Result<MemoryImportResponse, EngineError> {
    ws.memory_import(MemoryImportParams {
        content: content.into(),
        policy,
        source: Some("notes.jsonl".into()),
        visibility: Visibility::Group,
    })
}

fn get(ws: &Workspace, key: &str) -> Option<crate::engine::types::memory::MemoryEntry> {
    ws.memory_get(MemoryGetParams {
        key: key.into(),
        visibility: Visibility::Group,
    })
    .unwrap()
}

fn entry(key: &str, value: &str, created_at: i64, updated_at: i64) -> WireMemoryEntry {
    WireMemoryEntry {
        key: key.into(),
        value: value.into(),
        tags: vec!["t".into()],
        created_at,
        updated_at,
    }
}

fn file(entries: Vec<WireMemoryEntry>) -> String {
    render(entries, Visibility::Group).unwrap()
}

/// Notes exported from one repository and imported into another read back the same, and the
/// second repository exports the same bytes.
#[test]
fn an_export_imported_elsewhere_round_trips_byte_for_byte() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let from = workspace(a.path(), "fake", offline);
    put(&from, "decision/b", "second", &["x", "y"], Visibility::Group);
    put(&from, "decision/a", "first \"quoted\"\nline", &[], Visibility::Group);
    let exported = export(&from, Visibility::Group);
    assert_eq!(exported.notes, 2);

    let to = workspace(b.path(), "fake", offline);
    let report = import(&to, &exported.content, ImportPolicy::Newer).unwrap();
    assert_eq!(
        report,
        MemoryImportResponse {
            total: 2,
            added: 2,
            ..MemoryImportResponse::default()
        }
    );
    assert_eq!(export(&to, Visibility::Group).content, exported.content);
    assert_eq!(get(&to, "decision/a").unwrap().value, "first \"quoted\"\nline");
}

/// The file orders notes by key and starts with its header, whatever order they were saved in.
#[test]
fn an_export_is_a_header_then_notes_ordered_by_key() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "fake", offline);
    for key in ["c", "a", "b"] {
        put(&ws, key, key, &[], Visibility::Group);
    }
    let content = export(&ws, Visibility::Group).content;
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines[0],
        r#"{"format":"basemind-memory-export","version":1,"visibility":"group","notes":3}"#
    );
    let keys: Vec<String> = lines[1..]
        .iter()
        .map(|l| serde_json::from_str::<Line>(l).unwrap().key)
        .collect();
    assert_eq!(keys, ["a", "b", "c"]);
}

/// The export holds one tier: the other tier's notes stay out, and a file of one tier is refused by
/// the other.
#[test]
fn a_tier_is_exported_alone_and_imported_only_into_itself() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "fake", offline);
    put(&ws, "shared", "g", &[], Visibility::Group);
    put(&ws, "mine", "i", &[], Visibility::Individual);

    let group = export(&ws, Visibility::Group);
    let individual = export(&ws, Visibility::Individual);
    assert!(group.content.contains("shared") && !group.content.contains("mine"));
    assert!(individual.content.contains("mine") && !individual.content.contains("shared"));

    let error = import(&ws, &individual.content, ImportPolicy::Newer).unwrap_err();
    assert!(error.to_string().contains("target tier"), "{error}");
}

/// `--prefix` and `--tag` narrow an export to the notes that match.
#[test]
fn an_export_is_narrowed_by_prefix_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "fake", offline);
    put(&ws, "decision/a", "1", &["auth"], Visibility::Group);
    put(&ws, "decision/b", "2", &["db"], Visibility::Group);
    put(&ws, "finding/c", "3", &["auth"], Visibility::Group);
    let keys = |prefix: Option<&str>, tags: &[&str]| -> usize {
        ws.memory_export(MemoryExportParams {
            prefix: prefix.map(String::from),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            visibility: Visibility::Group,
        })
        .unwrap()
        .notes
    };
    assert_eq!(keys(None, &[]), 3);
    assert_eq!(keys(Some("decision/"), &[]), 2);
    assert_eq!(keys(None, &["auth"]), 2);
    assert_eq!(keys(Some("decision/"), &["auth"]), 1);
}

/// An export reads the notes of the index's scope and tier only: another scope's notes in the same
/// store are not in it.
#[test]
fn a_scan_reads_one_scope_and_tier_of_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let idx = IndexDb::open(dir.path()).unwrap();
    ops::put_core(&idx, "scope-a", 0, "", "k", "in a", &[]).unwrap();
    ops::put_core(&idx, "scope-b", 0, "", "k", "in b", &[]).unwrap();
    ops::put_core(&idx, "scope-a", 1, "me", "own", "individual", &[]).unwrap();

    let values = |scope: &str, vis: u8, owner: &str| -> Vec<String> {
        ops::scan_core(&idx, scope, vis, owner, "", &[], TRANSFER_MAX_BYTES)
            .unwrap()
            .into_iter()
            .map(|e| e.value)
            .collect()
    };
    assert_eq!(values("scope-a", 0, ""), ["in a"]);
    assert_eq!(values("scope-b", 0, ""), ["in b"]);
    assert_eq!(values("scope-a", 1, "me"), ["individual"]);
    assert!(values("scope-a", 1, "someone-else").is_empty());
    assert!(values("scope-c", 0, "").is_empty());
}

/// An export that would pass the byte cap is refused, not cut.
#[test]
fn a_scan_past_the_byte_cap_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let idx = IndexDb::open(dir.path()).unwrap();
    ops::put_core(&idx, "s", 0, "", "a", &"x".repeat(100), &[]).unwrap();
    ops::put_core(&idx, "s", 0, "", "b", &"x".repeat(100), &[]).unwrap();
    assert_eq!(ops::scan_core(&idx, "s", 0, "", "", &[], 300).unwrap().len(), 2);
    let error = ops::scan_core(&idx, "s", 0, "", "", &[], 150).unwrap_err();
    assert!(
        matches!(error, ops::MemoryOpError::TooLong { most: 150, .. }),
        "{error}"
    );
}

/// A record that cannot be read fails the scan instead of silently missing from the export.
#[test]
fn a_scan_over_an_unreadable_record_fails() {
    let dir = tempfile::tempdir().unwrap();
    let idx = IndexDb::open(dir.path()).unwrap();
    ops::put_core(&idx, "s", 0, "", "good", "v", &[]).unwrap();
    let raw_key = crate::index::keys::memory_by_key("s", 0, "", "bad");
    idx.memory()
        .unwrap()
        .notes_for_write()
        .unwrap()
        .insert(raw_key, b"not msgpack".to_vec())
        .unwrap();
    let error = ops::scan_core(&idx, "s", 0, "", "", &[], TRANSFER_MAX_BYTES).unwrap_err();
    assert!(matches!(error, ops::MemoryOpError::Unreadable { .. }), "{error}");
    assert!(error.to_string().contains("bad"), "{error}");
}

/// A store whose raw bytes fit the cap but whose rendered file does not is refused by the export
/// (an import would refuse the file).
#[test]
fn an_export_the_import_would_refuse_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "fake", offline);
    // A quote escapes to two bytes, so 60% of the cap in raw bytes renders to 120%.
    let quotes = TRANSFER_MAX_BYTES * 6 / 10;
    put(&ws, "a", &"\"".repeat(quotes / 2), &[], Visibility::Group);
    put(&ws, "b", &"\"".repeat(quotes / 2), &[], Visibility::Group);
    let error = ws
        .memory_export(MemoryExportParams::default())
        .expect_err("the rendered file is over the cap");
    assert!(error.to_string().contains("import reads at most"), "{error}");
}

/// A note an import would refuse by its tag count, or a count past the file cap, is refused by the
/// export, never dropped.
#[test]
fn an_export_refuses_notes_and_counts_the_import_would_refuse() {
    let mut many_tags = entry("a", "v", 1, 1);
    many_tags.tags = (0..=NOTE_MAX_TAGS).map(|n| format!("t{n}")).collect();
    let error = check_exportable(&[many_tags]).unwrap_err();
    assert!(error.to_string().contains("tags"), "{error}");
    let too_many: Vec<_> = (0..=TRANSFER_MAX_NOTES).map(|n| entry(&format!("k{n}"), "v", 1, 1)).collect();
    let error = check_exportable(&too_many).unwrap_err();
    assert!(error.to_string().contains("an import reads at most"), "{error}");
    check_exportable(&[entry("ok", "v", 1, 1)]).unwrap();
}

/// Under each policy, an existing note of `existing` and an imported one of `incoming` resolve as
/// named, and the stored note is the expected one.
#[test]
fn a_key_both_sides_hold_is_settled_by_the_policy() {
    use ImportPolicy::{Newer, Overwrite, Skip};
    // (policy, existing updated_at, incoming updated_at, outcome, value kept)
    let cases = [
        (Newer, 100, 200, "replaced", "incoming"),
        (Newer, 200, 100, "kept", "existing"),
        (Newer, 100, 100, "kept", "existing"),
        (Overwrite, 200, 100, "replaced", "incoming"),
        (Overwrite, 100, 200, "replaced", "incoming"),
        (Skip, 100, 200, "kept", "existing"),
        (Skip, 200, 100, "kept", "existing"),
    ];
    for (policy, existing, incoming, outcome, kept) in cases {
        let dir = tempfile::tempdir().unwrap();
        let idx = IndexDb::open(dir.path()).unwrap();
        let old = entry("k", "existing", 50, existing);
        ops::merge_core(&idx, "s", 0, "", &old, Newer, "seed").unwrap();
        let new = entry("k", "incoming", 70, incoming);

        let merged = ops::merge_core(&idx, "s", 0, "", &new, policy, "f").unwrap();

        let label = format!("{policy:?} {existing} {incoming}");
        let expected = if outcome == "replaced" {
            Merged::Replaced
        } else {
            Merged::Kept
        };
        assert_eq!(merged, expected, "{label}");
        let stored = ops::get_core(&idx, "s", 0, "", "k").unwrap().unwrap();
        assert_eq!(stored.value, kept, "{label}");
    }
}

/// A replaced note keeps the earlier `created_at`, takes the file's `updated_at`, and records the
/// import as its provenance; a note identical to the existing one changes nothing.
#[test]
fn a_replaced_note_keeps_its_earliest_creation_and_records_the_import() {
    let dir = tempfile::tempdir().unwrap();
    let idx = IndexDb::open(dir.path()).unwrap();
    ops::merge_core(
        &idx,
        "s",
        0,
        "",
        &entry("k", "old", 50, 100),
        ImportPolicy::Newer,
        "seed",
    )
    .unwrap();
    let incoming = entry("k", "new", 70, 200);

    let merged = ops::merge_core(&idx, "s", 0, "", &incoming, ImportPolicy::Newer, "notes.jsonl").unwrap();
    assert_eq!(merged, Merged::Replaced);
    let stored = ops::get_core(&idx, "s", 0, "", "k").unwrap().unwrap();
    assert_eq!((stored.created_at, stored.updated_at), (50, 200));

    let again = ops::merge_core(&idx, "s", 0, "", &incoming, ImportPolicy::Overwrite, "notes.jsonl").unwrap();
    assert_eq!(again, Merged::Unchanged);

    let notes = ops::scan_core(&idx, "s", 0, "", "", &[], TRANSFER_MAX_BYTES).unwrap();
    assert_eq!(notes.len(), 1);
}

/// The provenance of an imported note names the import and its source.
#[test]
fn an_imported_note_records_its_source_as_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "fake", offline);
    import(&ws, &file(vec![entry("k", "v", 1, 2)]), ImportPolicy::Newer).unwrap();

    let (vis_byte, owner) = ws.memory_namespace(Visibility::Group);
    let store = ws.store_read();
    let idx = ws.memory_index(&store).unwrap();
    let raw = crate::index::keys::memory_by_key(&ws.scope, vis_byte, owner, "k");
    let bytes = idx
        .memory()
        .unwrap()
        .notes()
        .unwrap()
        .unwrap()
        .get(raw)
        .unwrap()
        .unwrap();
    let record: crate::engine::types::memory::MemoryRecord = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(record.provenance.commands, ["memory import notes.jsonl"]);
}

/// Importing the same file twice is a no-op the second time.
#[test]
fn importing_a_file_twice_changes_nothing_the_second_time() {
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "fake", offline);
    let content = file(vec![entry("a", "1", 1, 2), entry("b", "2", 1, 2)]);
    assert_eq!(import(&ws, &content, ImportPolicy::Newer).unwrap().added, 2);
    let second = import(&ws, &content, ImportPolicy::Newer).unwrap();
    assert_eq!((second.added, second.replaced, second.unchanged), (0, 0, 2));
}

/// A file that is not a valid export is refused with the line and the problem, and nothing is
/// written, not even the notes before the bad line.
#[test]
fn a_bad_file_is_refused_whole_with_the_reason() {
    let header = |notes: usize| {
        format!(r#"{{"format":"basemind-memory-export","version":1,"visibility":"group","notes":{notes}}}"#)
    };
    let note = |key: &str| format!(r#"{{"key":"{key}","value":"v","tags":[],"created_at":1,"updated_at":2}}"#);
    let cases: Vec<(String, &str)> = vec![
        (String::new(), "empty"),
        ("not json\n".into(), "line 1"),
        (
            r#"{"format":"other","version":1,"visibility":"group","notes":0}"#.into(),
            "expected `basemind-memory-export`",
        ),
        (
            r#"{"format":"basemind-memory-export","version":2,"visibility":"group","notes":0}"#.into(),
            "newer basemind",
        ),
        (
            r#"{"format":"basemind-memory-export","version":0,"visibility":"group","notes":0}"#.into(),
            "unsupported version 0",
        ),
        (format!("{}\n{}\nbroken\n", header(2), note("a")), "line 3"),
        (format!("{}\n{}\n", header(2), note("a")), "truncated"),
        (format!("{}\n{}\n{}\n", header(1), note("a"), note("b")), "more notes"),
        (
            format!("{}\n{}\n{}\n", header(2), note("a"), note("a")),
            "appears twice",
        ),
        (format!("{}\n{}\n", header(1), note("")), "empty"),
        (
            format!(
                "{}\n{}\n",
                header(1),
                r#"{"key":"k","value":"v","tags":[],"created_at":-1,"updated_at":2}"#
            ),
            "negative timestamp",
        ),
        (
            format!(
                "{}\n{}\n",
                header(1),
                r#"{"key":"k","value":"v","tags":[],"created_at":1,"updated_at":2,"extra":1}"#
            ),
            "unknown field",
        ),
        (
            format!("{}\n{}\n", header(1), note(&"k".repeat(ops::KEY_MAX_BYTES + 1))),
            "at most 1024 bytes",
        ),
        (
            format!("{}\n", header(TRANSFER_MAX_NOTES + 1)),
            "at most 100000 are read",
        ),
        ("x".repeat(TRANSFER_MAX_BYTES + 1), "at most 8388608 are read"),
    ];
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path(), "fake", offline);
    for (content, expected) in cases {
        let error = import(&ws, &content, ImportPolicy::Newer).unwrap_err();
        assert!(matches!(error, EngineError::InvalidArgument(_)), "{expected}: {error}");
        assert!(error.to_string().contains(expected), "{expected}: {error}");
    }
    assert_eq!(export(&ws, Visibility::Group).notes, 0, "a refused file writes nothing");
}

/// A note with too many tags is refused.
#[test]
fn a_note_with_too_many_tags_is_refused() {
    let mut e = entry("k", "v", 1, 2);
    e.tags = (0..=NOTE_MAX_TAGS).map(|i| i.to_string()).collect();
    let error = parse(&file(vec![e])).unwrap_err();
    assert!(error.to_string().contains("tags"), "{error}");
}

/// A source label that is too long or holds a control character is refused; none is `file`.
#[test]
fn a_source_label_is_bounded() {
    assert_eq!(source_label(None).unwrap(), "file");
    assert_eq!(source_label(Some("  ")).unwrap(), "file");
    assert_eq!(source_label(Some(" a.jsonl ")).unwrap(), "a.jsonl");
    assert!(source_label(Some(&"s".repeat(SOURCE_MAX_CHARS + 1))).is_err());
    assert!(source_label(Some("a\nb")).is_err());
}

/// CRLF line endings and trailing blank lines (a file edited on another system) still import.
#[test]
fn crlf_and_trailing_blank_lines_are_accepted() {
    let content = file(vec![entry("k", "v", 1, 2)]).replace('\n', "\r\n") + "\r\n\r\n";
    let (tier, notes) = parse(&content).unwrap();
    assert_eq!((tier, notes.len()), (Visibility::Group, 1));
}
