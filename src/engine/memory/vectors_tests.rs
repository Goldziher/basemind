use super::*;
use crate::engine::memory::ops::{delete_core, put_core};
use crate::engine::memory::test_support::{DIM, workspace};
use crate::engine::types::memory::{MemoryDeleteParams, MemorySearchAnswer, MemorySearchToolParams};

/// The group tier's namespace byte.
fn vis() -> u8 {
    Visibility::Group.vis_byte()
}

fn hit(key: &str, _row_text: &str, updated_at: i64) -> MemoryHit {
    MemoryHit {
        key: key.into(),
        updated_at,
        distance: 0.5,
    }
}

fn note(key: &str, value: &str, updated_at: i64) -> WireMemoryEntry {
    WireMemoryEntry {
        key: key.into(),
        value: value.into(),
        tags: vec!["note".into()],
        created_at: 1,
        updated_at,
    }
}

fn notes_by_key(notes: Vec<(&str, Option<WireMemoryEntry>)>) -> AHashMap<String, Option<WireMemoryEntry>> {
    notes.into_iter().map(|(key, note)| (key.to_string(), note)).collect()
}

fn stamps(rows: &[(&str, i64)], duplicated: &[&str]) -> MemoryStamps {
    MemoryStamps {
        stamps: rows.iter().map(|(key, at)| (key.to_string(), *at)).collect(),
        duplicated: duplicated.iter().map(|key| key.to_string()).collect(),
    }
}

/// A unit vector that depends on the text alone, as every preset's is. Text containing
/// `unembeddable` is refused, as a model refuses input it cannot take.
fn fake_embed(text: &str) -> anyhow::Result<Vec<f32>> {
    if text.contains("unembeddable") {
        anyhow::bail!("the model rejects this text");
    }
    let mut vector = vec![0.0_f32; usize::from(DIM)];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % usize::from(DIM)] += f32::from(byte) / 255.0;
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    Ok(vector.into_iter().map(|x| x / norm).collect())
}

/// A workspace over an empty directory whose embedder is `model`, answering with [`fake_embed`].
fn server(root: &Path, model: &str) -> Workspace {
    workspace(root, model, fake_embed)
}

/// Save a note in the Fjall truth only: a note with no vector row, as after a wipe.
fn save(state: &Workspace, key: &str, value: &str) -> i64 {
    let store = state.store_read();
    let idx = store.index_db.as_ref().unwrap();
    put_core(idx, &state.scope, vis(), "", key, value, &["t".to_string()])
        .unwrap()
        .1
}

fn search_json(state: &Workspace, query: &str, limit: u32) -> serde_json::Value {
    let params = MemorySearchParams {
        query: query.into(),
        limit: Some(limit),
        tags: Vec::new(),
        visibility: Visibility::Group,
    };
    serde_json::to_value(search(state, params).unwrap()).unwrap()
}

fn hit_keys(response: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = response["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["key"].as_str().unwrap().to_string())
        .collect();
    keys.sort();
    keys
}

fn row(state: &Workspace, key: &str, value: &str, updated_at: i64) -> MemoryRow {
    MemoryRow {
        scope: state.scope.clone(),
        key: key.into(),
        tags: Vec::new(),
        visibility: "group".into(),
        agent_id: String::new(),
        embedding: fake_embed(value).unwrap(),
        created_at: 1,
        updated_at,
    }
}

/// Run a call into the vector store, which drives its own runtime.
fn off<T>(work: impl FnOnce() -> T) -> T {
    work()
}

fn rows_of(state: &Workspace) -> MemoryStamps {
    let lance = memory_lance_store(state).unwrap();
    let scope = state.scope.clone();
    off(move || lance.memory_stamps(&scope, "group", "").unwrap())
}

fn add_rows(state: &Workspace, rows: Vec<MemoryRow>) {
    let lance = memory_lance_store(state).unwrap();
    off(move || lance.add_memory_rows_unchecked(&rows).unwrap());
}

#[test]
fn notes_without_a_current_row_are_the_ones_to_embed() {
    let notes = || {
        vec![
            note("fresh", "a", 1),
            note("rewritten", "b", 9),
            note("rowless", "c", 3),
            note("twice", "d", 4),
            note("empty", "", 5),
        ]
    };
    let keys = |behind: Vec<WireMemoryEntry>| behind.into_iter().map(|note| note.key).collect::<Vec<_>>();

    assert_eq!(
        keys(notes_behind(notes(), &MemoryStamps::default())),
        ["fresh", "rewritten", "rowless", "twice"],
        "an empty table: every note that has text"
    );
    let rows = stamps(&[("fresh", 1), ("rewritten", 5), ("twice", 4)], &["twice"]);
    assert_eq!(keys(notes_behind(notes(), &rows)), ["rewritten", "rowless", "twice"]);
}

#[test]
fn the_store_is_per_repo_group_and_per_model() {
    crate::store::init_isolated_cache();
    let root = tempfile::tempdir().unwrap();
    let dir = memory_lance_dir(root.path(), "Balanced/v2", 384);

    let name = dir.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("balanced-v2-384-"), "{name}");
    assert_ne!(
        dir,
        memory_lance_dir(root.path(), "balanced-v2", 384),
        "two model names that read alike do not share a store"
    );
    assert_eq!(
        dir.parent().unwrap().parent(),
        crate::store::memory_store_dir(root.path()).parent(),
        "beside the notes, in the repo group directory"
    );
    assert!(!dir.starts_with(crate::store::workspace_cache_dir(root.path())));
    assert_ne!(dir, memory_lance_dir(root.path(), "fast", 384));
    assert_ne!(dir, memory_lance_dir(root.path(), "Balanced/v2", 768));
}

#[test]
fn a_row_whose_note_was_deleted_is_not_served() {
    let joined = truth_join(
        vec![hit("gone", "resurrected", 5)],
        &notes_by_key(vec![("gone", None)]),
        &[],
        10,
    );
    assert!(
        joined.hits.is_empty(),
        "a deleted note must not come back through its row"
    );
    assert_eq!(joined.dropped, 1);
}

#[test]
fn a_row_left_behind_by_a_newer_write_is_not_served() {
    let notes = notes_by_key(vec![("k", Some(note("k", "new text", 9)))]);
    let joined = truth_join(vec![hit("k", "old text", 5)], &notes, &[], 10);
    assert!(joined.hits.is_empty(), "the row holds the text of an earlier write");
    assert_eq!(joined.dropped, 1);
}

#[test]
fn a_current_row_is_answered_from_the_note() {
    let notes = notes_by_key(vec![("k", Some(note("k", "note text", 9)))]);
    let joined = truth_join(vec![hit("k", "row text", 9)], &notes, &[], 10);
    assert_eq!(joined.hits.len(), 1);
    assert_eq!(joined.hits[0].value, "note text");
    assert_eq!(joined.hits[0].tags, ["note"]);
    assert_eq!(joined.hits[0].distance, 0.5);
    assert_eq!(joined.dropped, 0);
}

#[test]
fn dropped_rows_do_not_count_against_the_limit() {
    let hits = vec![hit("stale", "x", 1), hit("a", "x", 2), hit("b", "x", 3)];
    let notes = notes_by_key(vec![
        ("stale", Some(note("stale", "y", 8))),
        ("a", Some(note("a", "a", 2))),
        ("b", Some(note("b", "b", 3))),
    ]);
    let joined = truth_join(hits, &notes, &[], 1);
    assert_eq!(joined.hits.len(), 1);
    assert_eq!(joined.hits[0].key, "a");
}

/// Two processes backfilling at once each insert a row for the same write of a note. Both rows
/// pass the stamp check; the note is served once, and the stale third row never.
#[test]
fn a_note_with_several_rows_is_served_once() {
    let hits = vec![
        hit("k", "x", 5),
        hit("k", "x", 9),
        hit("k", "x", 9),
        hit("other", "x", 2),
    ];
    let notes = notes_by_key(vec![
        ("k", Some(note("k", "text", 9))),
        ("other", Some(note("other", "o", 2))),
    ]);
    let joined = truth_join(hits, &notes, &[], 10);
    let keys: Vec<&str> = joined.hits.iter().map(|hit| hit.key.as_str()).collect();
    assert_eq!(keys, ["k", "other"]);
    assert_eq!(joined.dropped, 2, "the stale row and the second current one");
}

/// The `tags` filter reads the note's tags, the truth, keeps a note carrying any of them, and a row
/// it excludes is not a stale row.
#[test]
fn the_tags_filter_is_applied_to_the_note() {
    let notes = notes_by_key(vec![("k", Some(note("k", "text", 9)))]);
    let joined = |tags: &[&str]| {
        let tags: Vec<String> = tags.iter().map(|tag| tag.to_string()).collect();
        truth_join(vec![hit("k", "x", 9)], &notes, &tags, 10)
    };
    assert_eq!(joined(&["note"]).hits.len(), 1);
    assert_eq!(joined(&["row", "note"]).hits.len(), 1, "any of the tags is enough");
    let without = joined(&["row"]);
    assert!(without.hits.is_empty(), "`row` is a tag of the vector row only");
    assert_eq!(without.dropped, 0);
}

/// `memory_search` takes `tags`, as `memory_save` does: a search by meaning and a listing keep the
/// notes carrying any of them, and a `key` read refuses them.
#[test]
fn memory_search_keeps_the_notes_carrying_any_of_its_tags() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    for (key, tag) in [("auth", "security"), ("cache", "perf"), ("style", "ui")] {
        let store = state.store_read();
        let idx = store.index_db.as_ref().unwrap();
        put_core(idx, &state.scope, vis(), "", key, "a note", &[tag.to_string()]).unwrap();
    }
    let params = |query: Option<&str>, tags: &[&str]| MemorySearchToolParams {
        query: query.map(str::to_string),
        key: None,
        prefix: None,
        tags: Some(tags.iter().map(|tag| tag.to_string()).collect()),
        visibility: None,
        limit: None,
        cursor: None,
    };
    let keys = |answer: MemorySearchAnswer| -> Vec<String> {
        let mut keys: Vec<String> = match answer {
            MemorySearchAnswer::Search(found) => found.hits.into_iter().map(|hit| hit.key).collect(),
            MemorySearchAnswer::List(listed) => listed.entries.into_iter().map(|entry| entry.key).collect(),
            _ => panic!("a search or a listing"),
        };
        keys.sort_unstable();
        keys
    };

    let searched = state
        .memory_search(params(Some("a note"), &["security", "perf"]))
        .unwrap();
    assert_eq!(keys(searched), ["auth", "cache"]);
    let listed = state.memory_search(params(None, &["ui", "absent"])).unwrap();
    assert_eq!(keys(listed), ["style"]);
    assert!(keys(state.memory_search(params(None, &["absent"])).unwrap()).is_empty());

    let read = state
        .memory_search(MemorySearchToolParams {
            key: Some("auth".to_string()),
            ..params(None, &["security"])
        })
        .unwrap_err();
    assert!(
        read.to_string()
            .contains("a `key` read returns one note and takes no `tags`, `limit` or `cursor`"),
        "{read}"
    );
}

/// The whole path of a search by meaning: notes that have no row are embedded by the search that
/// looks for them, into the store of the repo group, and a row whose note is gone is not served.
#[test]
fn a_search_embeds_the_notes_it_finds_without_rows_and_answers_from_the_notes() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let saved_at = save(&state, "alpha", "the alpha note");
    save(&state, "beta", "the beta note");

    let first = search_json(&state, "the alpha note", 10);

    assert_eq!(hit_keys(&first), ["alpha", "beta"], "{first}");
    assert!(first.get("notice").is_none(), "two notes fit one call: {first}");
    let lance = memory_lance_store(&state).unwrap();
    assert_eq!(lance.dir(), memory_lance_dir(root.path(), "fake", DIM));
    assert_eq!(rows_of(&state).stamps["alpha"], saved_at);

    // A row for a note that does not exist, holding text of its own, and a stale row for `beta`.
    add_rows(
        &state,
        vec![row(&state, "ghost", "the alpha note", 7), row(&state, "beta", "old", 3)],
    );
    let second = search_json(&state, "the alpha note", 10);

    assert_eq!(hit_keys(&second), ["alpha", "beta"], "{second}");
    let beta = second["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["key"] == "beta")
        .unwrap();
    assert_eq!(
        beta["value"], "the beta note",
        "the value is the note's, never the row's"
    );
}

/// A search lists previews, as a prefix listing does: a note of megabytes is read whole by `key`,
/// never once per hit.
#[test]
fn a_search_hit_carries_a_preview_of_a_long_note() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let long = "word ".repeat(10_000);
    save(&state, "long", &long);

    let found = search_json(&state, "word", 10);

    let value = found["hits"][0]["value"].as_str().expect("a hit");
    assert_eq!(
        value.chars().count(),
        crate::engine::memory::ops::MEMORY_PREVIEW_CHARS + 1,
        "{found}"
    );
    assert!(value.ends_with('…'), "{value}");
}

/// Two worktrees of one clone configured with different presets share the group directory. Each
/// model has its own store there: opening one must leave the other's rows alone.
#[test]
fn two_embedding_models_keep_separate_stores_in_one_group() {
    let root = tempfile::tempdir().unwrap();
    let first = server(root.path(), "model-a");
    save(&first, "alpha", "the alpha note");
    assert_eq!(hit_keys(&search_json(&first, "alpha", 10)), ["alpha"]);
    let first_dir = memory_lance_store(&first).unwrap().dir().to_path_buf();
    drop(first);

    let second = server(root.path(), "model-b");
    assert_eq!(hit_keys(&search_json(&second, "alpha", 10)), ["alpha"]);
    let second_dir = memory_lance_store(&second).unwrap().dir().to_path_buf();
    drop(second);

    assert_ne!(first_dir, second_dir);
    let scope = format!("path:{}", root.path().display());
    let rows = off(move || {
        let reopened = LanceStore::open(&first_dir, DIM, "model-a").unwrap();
        reopened.memory_stamps(&scope, "group", "").unwrap()
    });
    assert_eq!(
        rows.stamps.keys().collect::<Vec<_>>(),
        ["alpha"],
        "the first model's rows survive the second model's open"
    );
}

/// One call embeds a bounded number of notes and says the index is behind; later calls continue
/// until every note has a row, and then say nothing.
#[test]
fn a_backfill_larger_than_one_call_continues_on_the_next() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let total = BACKFILL_NOTES_PER_CALL * 2 + 1;
    for index in 0..total {
        save(&state, &format!("note-{index:02}"), &format!("text {index}"));
    }

    let first = search_json(&state, "text", 100);
    assert_eq!(rows_of(&state).stamps.len(), BACKFILL_NOTES_PER_CALL);
    assert!(
        first["notice"]
            .as_str()
            .is_some_and(|notice| notice.contains("catching up")),
        "{first}"
    );

    let second = search_json(&state, "text", 100);
    assert_eq!(rows_of(&state).stamps.len(), BACKFILL_NOTES_PER_CALL * 2);
    assert!(second.get("notice").is_some(), "{second}");

    let third = search_json(&state, "text", 100);
    assert_eq!(hit_keys(&third).len(), total);
    assert!(third.get("notice").is_none(), "every note has a row: {third}");
}

/// A note the model refuses leaves the namespace behind: the next search tries it again instead of
/// treating the namespace as level for the life of the process.
#[test]
fn a_note_that_fails_to_embed_is_retried_by_the_next_search() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "good", "a good note");
    save(&state, "bad", "unembeddable text");

    let first = search_json(&state, "note", 10);
    assert_eq!(
        hit_keys(&first),
        ["good"],
        "the batch-mate of a refused note is still embedded"
    );
    assert!(first.get("notice").is_some(), "{first}");

    // The same key, saved with text the model takes, outside this process's put path.
    save(&state, "bad", "a mended note");
    let second = search_json(&state, "note", 10);

    assert_eq!(hit_keys(&second), ["bad", "good"], "{second}");
    assert!(second.get("notice").is_none(), "{second}");
}

/// Rows whose note is gone are removed by the backfill, and until then cannot push current rows
/// out of a search: with more orphans nearer the query than the limit over-fetches, the note is
/// still found.
#[test]
fn rows_without_a_note_are_pruned_and_cannot_starve_a_search() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "kept", "something far from the query");
    assert_eq!(hit_keys(&search_json(&state, "query text", 1)), ["kept"]);

    let orphans: Vec<MemoryRow> = (0..40)
        .map(|index| row(&state, &format!("orphan-{index}"), "query text", 5))
        .collect();
    add_rows(&state, orphans);

    // The namespace is marked level, so this search runs no backfill: forty rows nearer the query
    // than the note stand between it and a limit of one.
    let starved = search_json(&state, "query text", 1);
    assert_eq!(hit_keys(&starved), ["kept"], "{starved}");
    assert_eq!(rows_of(&state).stamps.len(), 41, "nothing pruned yet");

    // Refused rows un-mark the namespace; the next search prunes them.
    search_json(&state, "query text", 1);
    assert_eq!(rows_of(&state).stamps.keys().collect::<Vec<_>>(), ["kept"]);
}

/// A key with two rows (two processes embedding it at once) is rewritten to one by the backfill.
#[test]
fn duplicate_rows_are_rewritten_to_one() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let saved_at = save(&state, "twice", "a note");
    let twice = vec![
        row(&state, "twice", "a note", saved_at),
        row(&state, "twice", "a note", saved_at),
    ];
    add_rows(&state, twice);
    assert!(rows_of(&state).duplicated.contains("twice"));

    let response = search_json(&state, "a note", 10);

    assert_eq!(hit_keys(&response), ["twice"]);
    assert!(rows_of(&state).duplicated.is_empty());
}

/// A delete in a process that has not searched or saved yet still removes the row: the store is
/// opened for it.
#[test]
fn a_delete_removes_the_row_in_a_process_that_never_opened_the_store() {
    let root = tempfile::tempdir().unwrap();
    {
        let writer = server(root.path(), "fake");
        save(&writer, "doomed", "a note");
        assert_eq!(hit_keys(&search_json(&writer, "a note", 10)), ["doomed"]);
    }
    let state = server(root.path(), "fake");
    assert!(state.memory_lance.get().is_none());

    let params = MemoryDeleteParams {
        key: "doomed".into(),
        visibility: Visibility::Group,
    };
    state.memory_delete_note(params).unwrap();

    assert!(rows_of(&state).stamps.is_empty(), "the row went with the note");
}

/// A delete where no store was ever created does not create one.
#[test]
fn a_delete_does_not_create_a_store() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "plain", "a note");
    {
        let store = state.store_read();
        delete_core(store.index_db.as_ref().unwrap(), &state.scope, vis(), "", "plain").unwrap();
    }

    delete_vector_row(&state, Visibility::Group, "", "plain");

    assert!(!memory_lance_dir(root.path(), "fake", DIM).exists());
}

/// A note saved under one preset, then again under another, has a row in both stores. The delete
/// removes it from both, and leaves neither a marked row nor an earlier version that still holds it.
#[test]
fn a_delete_purges_the_note_from_every_models_store() {
    let root = tempfile::tempdir().unwrap();
    let first = server(root.path(), "model-a");
    put(&first, "doomed", "a note", true);
    put(&first, "kept", "another note", true);
    drop(first);
    let second = server(root.path(), "model-b");
    put(&second, "doomed", "a note", true);
    put(&second, "kept", "another note", true);

    let params = MemoryDeleteParams {
        key: "doomed".into(),
        visibility: Visibility::Group,
    };
    assert!(second.memory_delete_note(params).unwrap().deleted);
    assert_eq!(hit_keys(&search_json(&second, "a note", 10)), ["kept"]);
    drop(second);

    let scope = format!("path:{}", root.path().display());
    for model in ["model-a", "model-b"] {
        let dir = memory_lance_dir(root.path(), model, DIM);
        let scope = scope.clone();
        let (keys, marked, versions) = off(move || {
            let store = LanceStore::open(&dir, DIM, model).unwrap();
            let keys: Vec<String> = store
                .memory_stamps(&scope, "group", "")
                .unwrap()
                .stamps
                .keys()
                .cloned()
                .collect();
            let versions = store.version_counts().unwrap();
            (keys, store.memory_rows_marked_deleted().unwrap(), versions)
        });
        assert_eq!(keys, ["kept"], "{model}");
        assert_eq!(marked, Some(0), "{model}");
        let memory_versions: Vec<usize> = versions
            .into_iter()
            .filter(|(table, _)| table == "memory")
            .map(|(_, count)| count)
            .collect();
        assert_eq!(memory_versions, [1], "{model}");
    }
}

/// One axis per topic: notes on different topics are orthogonal, at distance 2 from each other.
fn topic_embed(text: &str) -> anyhow::Result<Vec<f32>> {
    let axis = ["alpha", "beta", "gamma"]
        .iter()
        .position(|topic| text.contains(topic))
        .unwrap_or(3);
    let mut vector = vec![0.0_f32; usize::from(DIM)];
    vector[axis] = 1.0;
    Ok(vector)
}

/// A search answers the notes within the relevance floor and says how many nearer ones it left
/// out; a query about none of the notes is answered with none.
#[test]
fn a_search_leaves_out_the_notes_past_the_relevance_floor() {
    let root = tempfile::tempdir().unwrap();
    let state = workspace(root.path(), "fake", topic_embed);
    save(&state, "a", "the alpha note");
    save(&state, "b", "the beta note");

    let related = search_json(&state, "alpha", 10);
    assert_eq!(hit_keys(&related), ["a"]);
    assert_eq!(related["hits"][0]["distance"], 0.0);
    assert_eq!(related["notice"], unrelated_notice(1));
    assert!(related.get("lexical").is_none(), "{related}");

    let unrelated = search_json(&state, "gamma", 10);
    assert!(hit_keys(&unrelated).is_empty(), "{unrelated}");
    assert_eq!(unrelated["notice"], unrelated_notice(2));
}

#[test]
fn the_relevance_floor_notice_names_the_count_and_the_floor() {
    assert_eq!(
        unrelated_notice(3),
        "3 of the nearest notes are farther from the query than the relevance floor (distance 1.0) and are left \
         out: they are unlikely to be about it. A listing by `prefix` or `tag` shows every note"
    );
}

fn put_params(key: &str, value: &str, embed: bool) -> crate::engine::types::memory::MemoryPutParams {
    crate::engine::types::memory::MemoryPutParams {
        key: key.into(),
        value: value.into(),
        tags: None,
        embed,
        visibility: Visibility::Group,
    }
}

fn put(state: &Workspace, key: &str, value: &str, embed: bool) -> serde_json::Value {
    serde_json::to_value(state.memory_put(put_params(key, value, embed)).unwrap()).unwrap()
}

fn mark_of(state: &Workspace) -> Option<Backfill> {
    backfill_marks(state).get(&(vis(), String::new())).copied()
}

/// A note is saved again while the backfill is embedding the text it listed. The row the backfill
/// then writes is of the earlier write: it must not replace the row the save wrote, and the pass
/// must not call the namespace level over it.
#[test]
fn a_note_saved_again_while_its_backfill_embeds_keeps_its_new_row_and_is_not_called_level() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "k", "the old text");
    test_hooks::once_while_embedding(
        &state.scope,
        Box::new(|state| {
            let saved = put(state, "k", "the new text", true);
            assert_eq!(saved["embedded"], true, "{saved}");
        }),
    );

    let during = search_json(&state, "the new text", 10);

    let hit = during["hits"].as_array().unwrap().first().expect("the note is found");
    assert_eq!(
        (hit["key"].as_str(), hit["value"].as_str()),
        (Some("k"), Some("the new text")),
        "the save's row answers, with the note's text: {during}"
    );
    assert!(
        during.get("notice").is_some(),
        "the pass wrote a row that is behind its note, so the rows are not level: {during}"
    );
    assert_eq!(mark_of(&state), None);

    let after = search_json(&state, "the new text", 10);
    assert_eq!(hit_keys(&after), ["k"]);
    assert!(after.get("notice").is_none(), "{after}");
    let rows = rows_of(&state);
    assert!(rows.duplicated.is_empty(), "the row of the earlier write is gone");
    let note = state.memory_get_wire(vis(), "", "k").unwrap().unwrap();
    assert_eq!(rows.stamps["k"], note.updated_at);
}

/// Level is what this process last saw. Another process may have saved a note with no row since,
/// so the belief lapses and a search looks again.
#[test]
fn a_level_mark_lapses_and_the_next_search_reconciles_again() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "first", "a note");
    search_json(&state, "a note", 10);
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));
    // Saved outside this process's save path, as by another process whose embedding failed.
    save(&state, "second", "a note too");

    assert_eq!(
        hit_keys(&search_json(&state, "a note", 10)),
        ["first"],
        "believed level: no listing, so the rowless note is not seen"
    );
    let owed = rows_owed_here(&state);
    backfill_marks(&state).insert(
        (vis(), String::new()),
        Backfill::Level {
            until: Instant::now(),
            ttl: LEVEL_TTL,
            owed,
        },
    );

    assert_eq!(hit_keys(&search_json(&state, "a note", 10)), ["first", "second"]);
}

/// Another process saved a note it could not embed, or one saved with `embed: false`, while this
/// one believes the namespace level. Its save says so in the group's stamp, and the next search
/// here embeds the note without waiting for the belief to lapse.
#[test]
fn a_rowless_save_in_another_process_ends_level_here() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "first", "a note");
    search_json(&state, "a note", 10);
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));

    // What another process's save does when it writes no row: the note, then the stamp.
    save(&state, "second", "a note too");
    owe_rows(root.path());

    assert_eq!(hit_keys(&search_json(&state, "a note", 10)), ["first", "second"]);
    let stamp = rows_owed_here(&state);
    assert!(
        matches!(mark_of(&state), Some(Backfill::Level { owed, .. }) if owed == stamp),
        "level again, as of the stamp it read"
    );
}

/// The stamp other processes read changes with every save that wrote no row, and only with those.
#[test]
fn a_save_without_a_row_rewrites_the_group_stamp() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let untouched = rows_owed_here(&state);
    put(&state, "embedded", "a note", true);
    assert_eq!(rows_owed_here(&state), untouched, "a save with its row owes nothing");

    put(&state, "deferred", "a note", false);
    let once = rows_owed_here(&state);
    assert_ne!(once, untouched);
    put(&state, "refused", "unembeddable text", true);
    assert_ne!(rows_owed_here(&state), once, "every rowless save is a new stamp");
}

/// A stamp is renamed into place whole, so a reader never finds it empty, and nothing written
/// aside is left next to it.
#[test]
fn a_stamp_is_renamed_into_place_and_leaves_nothing_aside() {
    let root = tempfile::tempdir().unwrap();
    owe_rows(root.path());
    let first = rows_owed(root.path(), "table");
    owe_rows(root.path());
    assert_ne!(rows_owed(root.path(), "table"), first, "each save is a new stamp");
    let mut names: Vec<String> = std::fs::read_dir(crate::store_layout::repo_group_dir(root.path()))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [ROWS_OWED_FILE]);
}

/// Worktrees of one group configured with different presets search different tables. A save that
/// wrote its row in its own preset's table wrote none in the others, and a process that believes
/// another table level hears of the note at its next search, not when the belief lapses.
#[test]
fn a_save_with_its_row_in_one_preset_ends_level_in_another() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = server(root.path(), "model-a");
    put(&elsewhere, "first", "a note", true);
    search_json(&elsewhere, "a note", 10);
    let level = mark_of(&elsewhere).expect("a mark");
    assert!(matches!(level, Backfill::Level { .. }));
    drop(elsewhere);

    let saver = server(root.path(), "model-b");
    let own = rows_owed_here(&saver);
    assert_eq!(put(&saver, "second", "a note too", true)["embedded"], true);
    assert_eq!(rows_owed_here(&saver), own, "the saver's own table owes nothing");
    drop(saver);

    let elsewhere = server(root.path(), "model-a");
    backfill_marks(&elsewhere).insert((vis(), String::new()), level);
    assert_eq!(hit_keys(&search_json(&elsewhere, "a note", 10)), ["first", "second"]);
}

/// A save that wrote no row leaves a note the rows are behind: the embedding failed, or the save
/// asked for none. Either ends the belief that the namespace is level, and the next search by
/// meaning embeds the note.
#[test]
fn a_save_that_wrote_no_row_ends_level_and_the_next_search_embeds_the_note() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    put(&state, "seed", "a seed note", true);
    search_json(&state, "a note", 10);
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));

    let deferred = put(&state, "later", "a note for later", false);
    assert_eq!(deferred["embedded"], false);
    assert_eq!(mark_of(&state), None, "a save without a row ends level");
    assert_eq!(hit_keys(&search_json(&state, "a note", 10)), ["later", "seed"]);
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));

    let failed = put(&state, "refused", "unembeddable text", true);
    assert_eq!(failed["embedded"], false, "{failed}");
    assert!(
        failed["notice"]
            .as_str()
            .is_some_and(|notice| notice.contains("is saved"))
    );
    assert_eq!(mark_of(&state), None, "a failed embedding ends level");
}

/// A note rewritten to no text keeps no row: the save removes it, and a row left by a save made
/// elsewhere is pruned by the backfill instead of being refused by every search that meets it.
#[test]
fn the_row_of_a_note_emptied_of_its_text_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    put(&state, "here", "some text", true);
    put(&state, "elsewhere", "some text", true);
    assert_eq!(rows_of(&state).stamps.len(), 2);

    put(&state, "here", "", false);
    assert_eq!(
        rows_of(&state).stamps.keys().collect::<Vec<_>>(),
        ["elsewhere"],
        "the save that emptied the note removed its row"
    );

    save(&state, "elsewhere", "");
    backfill_marks(&state).clear();
    search_json(&state, "some text", 10);
    assert!(rows_of(&state).stamps.is_empty(), "the backfill prunes it");
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })), "and is level");
}

/// A join over more notes than one request may carry is read in several requests.
#[test]
fn notes_beyond_one_request_are_read_in_chunks() {
    use crate::engine::memory::ops::GET_MANY_MAX_KEYS;

    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "present", "a note");
    let mut keys: Vec<String> = (0..GET_MANY_MAX_KEYS * 2 + 3)
        .map(|index| format!("absent-{index}"))
        .collect();
    keys.push("present".to_string());

    let read = state.memory_get_many_wire(vis(), "", keys.clone()).unwrap();

    assert_eq!(read.len(), keys.len(), "one slot per key, in order");
    assert_eq!(
        read.last().unwrap().as_ref().map(|note| note.key.as_str()),
        Some("present")
    );
    assert!(read[..keys.len() - 1].iter().all(Option::is_none));
}

/// Notes longer together than one reply may carry are read over several replies, each starting
/// at the first key the one before left unanswered.
#[test]
fn notes_beyond_one_reply_are_read_in_several() {
    use crate::engine::memory::ops::{GET_MANY_MAX_BYTES, NOTE_MAX_BYTES};

    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    // The longest note a save takes, with its one-byte tag: two of them, with their keys and
    // framing, pass one reply. ~keep
    let long = "x".repeat(NOTE_MAX_BYTES - 1);
    const { assert!(2 * NOTE_MAX_BYTES >= GET_MANY_MAX_BYTES) };
    let keys: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
    for key in &keys {
        save(&state, key, &long);
    }

    let read = state.memory_get_many_wire(vis(), "", keys.clone()).unwrap();

    let read_keys: Vec<Option<&str>> = read.iter().map(|note| note.as_ref().map(|n| n.key.as_str())).collect();
    assert_eq!(read_keys, [Some("a"), Some("b"), Some("c")]);
    assert!(read.iter().flatten().all(|note| note.value.len() == long.len()));
}

/// Stamps decide which rows are current. Only the notes of current rows are read whole, and a
/// current row whose note was not read is neither served nor refused.
#[test]
fn stamps_pick_the_notes_to_read_and_the_rows_to_refuse() {
    let hits = vec![
        hit("a", "", 5),
        hit("stale", "", 1),
        hit("a", "", 5),
        hit("c", "", 7),
        hit("gone", "", 3),
    ];
    let stamps: AHashMap<String, Option<i64>> = [("a", Some(5)), ("stale", Some(2)), ("c", Some(7)), ("gone", None)]
        .into_iter()
        .map(|(key, stamp)| (key.to_string(), stamp))
        .collect();

    assert_eq!(current_keys(&hits, &stamps), ["a", "c"]);

    let notes = notes_by_key(vec![("a", Some(note("a", "text", 5)))]);
    let keys: Vec<String> = decidable(&hits, &stamps, &notes)
        .into_iter()
        .map(|hit| hit.key)
        .collect();
    assert_eq!(keys, ["a", "stale", "a", "gone"], "c is current but unread");
    let joined = truth_join(decidable(&hits, &stamps, &notes), &notes, &[], 10);
    assert_eq!(joined.hits.len(), 1);
    assert_eq!(
        joined.dropped, 3,
        "the stale row, the second row of a, the row of a gone note"
    );
}

/// A look after a lapse that finds the namespace level costs a listing of every note and row, so
/// the next one waits longer; one that found work starts over.
#[test]
fn each_look_that_finds_nothing_to_do_waits_longer_up_to_a_bound() {
    assert_eq!(level_ttl(None, false), LEVEL_TTL, "the first look");
    assert_eq!(level_ttl(Some(LEVEL_TTL), false), LEVEL_TTL * 2);
    assert_eq!(level_ttl(Some(LEVEL_TTL * 2), false), LEVEL_TTL * 4);
    assert_eq!(level_ttl(Some(LEVEL_TTL_MAX), false), LEVEL_TTL_MAX);
    assert_eq!(
        level_ttl(Some(LEVEL_TTL_MAX), true),
        LEVEL_TTL,
        "work found starts over"
    );
}

/// The doubled wait reaches the mark a lapsed search leaves.
#[test]
fn a_lapsed_look_that_finds_nothing_doubles_the_wait() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "k", "a note");
    search_json(&state, "note", 5);
    assert!(matches!(mark_of(&state), Some(Backfill::Level { ttl, .. }) if ttl == LEVEL_TTL));
    let owed = rows_owed_here(&state);
    backfill_marks(&state).insert(
        (vis(), String::new()),
        Backfill::Level {
            until: Instant::now(),
            ttl: LEVEL_TTL,
            owed,
        },
    );

    search_json(&state, "note", 5);

    assert!(matches!(mark_of(&state), Some(Backfill::Level { ttl, .. }) if ttl == LEVEL_TTL * 2));
}

/// A tag filter that refuses most notes read leaves a search short by a few notes round after
/// round: what it reads grows geometrically, so the rounds grow with the log of what it reads.
#[test]
fn a_search_short_of_its_limit_reads_at_least_twice_as_far_each_round() {
    assert_eq!(read_further(10, 10, 9), 20, "one note short still doubles");
    assert_eq!(read_further(10, 10, 0), 20);
    assert_eq!(read_further(5, 100, 0), 105, "the shortfall when that is more");
    let mut wanted = 10;
    let mut rounds = 0;
    while wanted < 10_000 {
        wanted = read_further(wanted, 10, 9);
        rounds += 1;
    }
    assert!(rounds <= 10, "{rounds} rounds to read 10,000 notes");
}

/// A store that would not open (here a file in its place; in the field another open holding its
/// lock past the wait) answers the calls after it at once, as unavailable, instead of each one
/// trying again and waiting out the same holder.
#[test]
fn a_store_that_would_not_open_is_unavailable_at_once_to_the_next_call() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "k", "a note");
    let dir = memory_lance_dir(root.path(), "fake", DIM);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();
    let params = || MemorySearchParams {
        query: "a note".into(),
        limit: Some(5),
        tags: Vec::new(),
        visibility: Visibility::Group,
    };
    let first = search(&state, params()).unwrap_err();
    let EngineError::Unavailable(message) = &first else {
        panic!("a refusal, not an internal error: {first:?}");
    };
    assert!(
        message.starts_with("`memory_search`: vector store unavailable"),
        "{message}"
    );

    std::fs::remove_file(&dir).unwrap();
    let second = search(&state, params()).unwrap_err();
    assert!(second.to_string().contains("unavailable"), "{second}");
    assert!(!dir.exists(), "the store was not opened again");
}
