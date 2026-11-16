use std::sync::Arc;

use super::*;
use crate::embeddings::SharedEmbedder;
use crate::mcp::memory::{run_memory_delete, run_memory_search};
use crate::mcp::memory_ops::{delete_core, put_core};
use crate::mcp::types_memory::MemoryDeleteParams;

const DIM: u16 = 4;
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

/// A vector that depends on the text alone. Text containing `unembeddable` is refused, as a model
/// refuses input it cannot take.
fn fake_embed(text: &str) -> anyhow::Result<Vec<f32>> {
    if text.contains("unembeddable") {
        anyhow::bail!("the model rejects this text");
    }
    let mut vector = vec![0.0_f32; usize::from(DIM)];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % usize::from(DIM)] += f32::from(byte) / 255.0;
    }
    Ok(vector)
}

/// A server over an empty workspace whose embedder is `model`, answering with [`fake_embed`].
fn server(root: &Path, model: &str) -> ServerState {
    use crate::config::Config;
    use crate::store::{Store, VIEW_WORKING};

    crate::store::init_isolated_cache();
    // An earlier server over the same root releases its index lock a moment after it is dropped.
    let opening = std::time::Instant::now();
    let store = loop {
        match Store::open(root, VIEW_WORKING) {
            Ok(store) => break store,
            Err(_) if opening.elapsed() < Duration::from_secs(20) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => panic!("open the store: {error}"),
        }
    };
    let shared = crate::mcp::SharedReadStack::new(
        store,
        root.to_path_buf(),
        Arc::new(Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).unwrap()),
        None,
        crate::mcp::ServerOptions::default(),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        None,
    );
    shared
        .embedder
        .set(Arc::new(SharedEmbedder::fake(model, DIM, fake_embed)))
        .ok()
        .unwrap();
    ServerState::for_connection(Arc::new(shared), "test".into())
}

/// Save a note in the Fjall truth only: a note with no vector row, as after a wipe.
async fn save(state: &ServerState, key: &str, value: &str) -> i64 {
    let store = state.shared.store.read().await;
    let idx = store.index_db.as_ref().unwrap();
    put_core(idx, &state.shared.scope, vis(), "", key, value, &["t".to_string()])
        .unwrap()
        .1
}

async fn search_json(state: &ServerState, query: &str, limit: u32) -> serde_json::Value {
    let params = MemorySearchParams {
        query: query.into(),
        limit: Some(limit),
        tag: None,
        visibility: Visibility::Group,
    };
    run_memory_search(state, params)
        .await
        .unwrap()
        .structured_content
        .unwrap()
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

fn row(state: &ServerState, key: &str, value: &str, updated_at: i64) -> MemoryRow {
    MemoryRow {
        scope: state.shared.scope.clone(),
        key: key.into(),
        tags: Vec::new(),
        visibility: "group".into(),
        agent_id: String::new(),
        embedding: fake_embed(value).unwrap(),
        created_at: 1,
        updated_at,
    }
}

/// Run a call into the vector store on a blocking thread: the store drives its own runtime.
async fn off<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work).await.unwrap()
}

async fn rows_of(state: &ServerState) -> MemoryStamps {
    let lance = memory_lance_store(state).await.unwrap();
    let scope = state.shared.scope.clone();
    off(move || lance.memory_stamps(&scope, "group", "").unwrap()).await
}

async fn add_rows(state: &ServerState, rows: Vec<MemoryRow>) {
    let lance = memory_lance_store(state).await.unwrap();
    off(move || lance.add_memory_rows_unchecked(&rows).unwrap()).await;
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
        None,
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
    let joined = truth_join(vec![hit("k", "old text", 5)], &notes, None, 10);
    assert!(joined.hits.is_empty(), "the row holds the text of an earlier write");
    assert_eq!(joined.dropped, 1);
}

#[test]
fn a_current_row_is_answered_from_the_note() {
    let notes = notes_by_key(vec![("k", Some(note("k", "note text", 9)))]);
    let joined = truth_join(vec![hit("k", "row text", 9)], &notes, None, 10);
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
    let joined = truth_join(hits, &notes, None, 1);
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
    let joined = truth_join(hits, &notes, None, 10);
    let keys: Vec<&str> = joined.hits.iter().map(|hit| hit.key.as_str()).collect();
    assert_eq!(keys, ["k", "other"]);
    assert_eq!(joined.dropped, 2, "the stale row and the second current one");
}

/// The tag filter reads the note's tags, the truth, and a row it excludes is not a stale row.
#[test]
fn the_tag_filter_is_applied_to_the_note() {
    let notes = notes_by_key(vec![("k", Some(note("k", "text", 9)))]);
    let with = truth_join(vec![hit("k", "x", 9)], &notes, Some("note"), 10);
    let without = truth_join(vec![hit("k", "x", 9)], &notes, Some("row"), 10);
    assert_eq!(with.hits.len(), 1);
    assert!(without.hits.is_empty(), "`row` is a tag of the vector row only");
    assert_eq!(without.dropped, 0);
}

/// The whole path of a search by meaning: notes that have no row are embedded by the search that
/// looks for them, into the store of the repo group, and a row whose note is gone is not served.
#[tokio::test]
async fn a_search_embeds_the_notes_it_finds_without_rows_and_answers_from_the_notes() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let saved_at = save(&state, "alpha", "the alpha note").await;
    save(&state, "beta", "the beta note").await;

    let first = search_json(&state, "the alpha note", 10).await;

    assert_eq!(hit_keys(&first), ["alpha", "beta"], "{first}");
    assert!(first.get("notice").is_none(), "two notes fit one call: {first}");
    let lance = memory_lance_store(&state).await.unwrap();
    assert_eq!(lance.dir(), memory_lance_dir(root.path(), "fake", DIM));
    assert_eq!(rows_of(&state).await.stamps["alpha"], saved_at);

    // A row for a note that does not exist, holding text of its own, and a stale row for `beta`.
    add_rows(
        &state,
        vec![row(&state, "ghost", "the alpha note", 7), row(&state, "beta", "old", 3)],
    )
    .await;
    let second = search_json(&state, "the alpha note", 10).await;

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

/// Two worktrees of one clone configured with different presets share the group directory. Each
/// model has its own store there: opening one must leave the other's rows alone.
#[tokio::test]
async fn two_embedding_models_keep_separate_stores_in_one_group() {
    let root = tempfile::tempdir().unwrap();
    let first = server(root.path(), "model-a");
    save(&first, "alpha", "the alpha note").await;
    assert_eq!(hit_keys(&search_json(&first, "alpha", 10).await), ["alpha"]);
    let first_dir = memory_lance_store(&first).await.unwrap().dir().to_path_buf();
    drop(first);

    let second = server(root.path(), "model-b");
    assert_eq!(hit_keys(&search_json(&second, "alpha", 10).await), ["alpha"]);
    let second_dir = memory_lance_store(&second).await.unwrap().dir().to_path_buf();
    drop(second);

    assert_ne!(first_dir, second_dir);
    let scope = format!("path:{}", root.path().display());
    let rows = off(move || {
        let reopened = LanceStore::open(&first_dir, DIM, "model-a").unwrap();
        reopened.memory_stamps(&scope, "group", "").unwrap()
    })
    .await;
    assert_eq!(
        rows.stamps.keys().collect::<Vec<_>>(),
        ["alpha"],
        "the first model's rows survive the second model's open"
    );
}

/// One call embeds a bounded number of notes and says the index is behind; later calls continue
/// until every note has a row, and then say nothing.
#[tokio::test]
async fn a_backfill_larger_than_one_call_continues_on_the_next() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let total = BACKFILL_NOTES_PER_CALL * 2 + 1;
    for index in 0..total {
        save(&state, &format!("note-{index:02}"), &format!("text {index}")).await;
    }

    let first = search_json(&state, "text", 100).await;
    assert_eq!(rows_of(&state).await.stamps.len(), BACKFILL_NOTES_PER_CALL);
    assert!(
        first["notice"]
            .as_str()
            .is_some_and(|notice| notice.contains("catching up")),
        "{first}"
    );

    let second = search_json(&state, "text", 100).await;
    assert_eq!(rows_of(&state).await.stamps.len(), BACKFILL_NOTES_PER_CALL * 2);
    assert!(second.get("notice").is_some(), "{second}");

    let third = search_json(&state, "text", 100).await;
    assert_eq!(hit_keys(&third).len(), total);
    assert!(third.get("notice").is_none(), "every note has a row: {third}");
}

/// A note the model refuses leaves the namespace behind: the next search tries it again instead of
/// treating the namespace as level for the life of the process.
#[tokio::test]
async fn a_note_that_fails_to_embed_is_retried_by_the_next_search() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "good", "a good note").await;
    save(&state, "bad", "unembeddable text").await;

    let first = search_json(&state, "note", 10).await;
    assert_eq!(
        hit_keys(&first),
        ["good"],
        "the batch-mate of a refused note is still embedded"
    );
    assert!(first.get("notice").is_some(), "{first}");

    // The same key, saved with text the model takes, outside this process's put path.
    save(&state, "bad", "a mended note").await;
    let second = search_json(&state, "note", 10).await;

    assert_eq!(hit_keys(&second), ["bad", "good"], "{second}");
    assert!(second.get("notice").is_none(), "{second}");
}

/// Rows whose note is gone are removed by the backfill, and until then cannot push current rows
/// out of a search: with more orphans nearer the query than the limit over-fetches, the note is
/// still found.
#[tokio::test]
async fn rows_without_a_note_are_pruned_and_cannot_starve_a_search() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "kept", "something far from the query").await;
    assert_eq!(hit_keys(&search_json(&state, "query text", 1).await), ["kept"]);

    let orphans: Vec<MemoryRow> = (0..40)
        .map(|index| row(&state, &format!("orphan-{index}"), "query text", 5))
        .collect();
    add_rows(&state, orphans).await;

    // The namespace is marked level, so this search runs no backfill: forty rows nearer the query
    // than the note stand between it and a limit of one.
    let starved = search_json(&state, "query text", 1).await;
    assert_eq!(hit_keys(&starved), ["kept"], "{starved}");
    assert_eq!(rows_of(&state).await.stamps.len(), 41, "nothing pruned yet");

    // Refused rows un-mark the namespace; the next search prunes them.
    search_json(&state, "query text", 1).await;
    assert_eq!(rows_of(&state).await.stamps.keys().collect::<Vec<_>>(), ["kept"]);
}

/// A key with two rows (two processes embedding it at once) is rewritten to one by the backfill.
#[tokio::test]
async fn duplicate_rows_are_rewritten_to_one() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let saved_at = save(&state, "twice", "a note").await;
    let twice = vec![
        row(&state, "twice", "a note", saved_at),
        row(&state, "twice", "a note", saved_at),
    ];
    add_rows(&state, twice).await;
    assert!(rows_of(&state).await.duplicated.contains("twice"));

    let response = search_json(&state, "a note", 10).await;

    assert_eq!(hit_keys(&response), ["twice"]);
    assert!(rows_of(&state).await.duplicated.is_empty());
}

/// A delete in a process that has not searched or saved yet still removes the row: the store is
/// opened for it.
#[tokio::test]
async fn a_delete_removes_the_row_in_a_process_that_never_opened_the_store() {
    let root = tempfile::tempdir().unwrap();
    {
        let writer = server(root.path(), "fake");
        save(&writer, "doomed", "a note").await;
        assert_eq!(hit_keys(&search_json(&writer, "a note", 10).await), ["doomed"]);
    }
    let state = server(root.path(), "fake");
    assert!(state.shared.memory_lance.get().is_none());

    let params = MemoryDeleteParams {
        key: "doomed".into(),
        visibility: Visibility::Group,
    };
    run_memory_delete(&state, params).await.unwrap();

    assert!(rows_of(&state).await.stamps.is_empty(), "the row went with the note");
}

/// A delete where no store was ever created does not create one.
#[tokio::test]
async fn a_delete_does_not_create_a_store() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "plain", "a note").await;
    {
        let store = state.shared.store.read().await;
        delete_core(
            store.index_db.as_ref().unwrap(),
            &state.shared.scope,
            vis(),
            "",
            "plain",
        )
        .unwrap();
    }

    delete_vector_row(&state, Visibility::Group, "", "plain").await;

    assert!(!memory_lance_dir(root.path(), "fake", DIM).exists());
}

/// A save and a delete fsync the notes store. On the server's multi-thread runtime that runs
/// through `block_in_place`; on a current-thread one, inline. Both must work.
#[tokio::test(flavor = "multi_thread")]
async fn a_save_and_a_delete_work_on_a_multi_thread_runtime() {
    use crate::mcp::memory::run_memory_put;
    use crate::mcp::types_memory::MemoryPutParams;

    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let saved = run_memory_put(
        &state,
        MemoryPutParams {
            key: "k".into(),
            value: "a saved note".into(),
            tags: None,
            embed: true,
            visibility: Visibility::Group,
        },
    )
    .await
    .unwrap()
    .structured_content
    .unwrap();
    assert_eq!(saved["embedded"], true, "{saved}");
    assert_eq!(hit_keys(&search_json(&state, "a saved note", 10).await), ["k"]);

    let params = MemoryDeleteParams {
        key: "k".into(),
        visibility: Visibility::Group,
    };
    run_memory_delete(&state, params).await.unwrap();
    assert!(hit_keys(&search_json(&state, "a saved note", 10).await).is_empty());
    assert!(rows_of(&state).await.stamps.is_empty());
}

/// `blocking` is what keeps a save's and a delete's fsync off the reactor. With one worker thread,
/// a task spawned beside the blocked one can only run if the worker hands its runtime over first;
/// run inline, the call would hold the only worker for as long as it waits for that task.
#[test]
fn blocking_work_does_not_hold_the_only_worker_of_a_multi_thread_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let ran_beside = runtime.block_on(async {
        tokio::spawn(async {
            let (ran, saw_it) = std::sync::mpsc::channel();
            tokio::spawn(async move {
                let _ = ran.send(());
            });
            crate::mcp::index_route::blocking(|| saw_it.recv_timeout(Duration::from_secs(10)).is_ok())
        })
        .await
        .unwrap()
    });
    assert!(ran_beside, "a task beside the blocking call never ran");
}

fn put_params(key: &str, value: &str, embed: bool) -> crate::mcp::types_memory::MemoryPutParams {
    crate::mcp::types_memory::MemoryPutParams {
        key: key.into(),
        value: value.into(),
        tags: None,
        embed,
        visibility: Visibility::Group,
    }
}

async fn put(state: &ServerState, key: &str, value: &str, embed: bool) -> serde_json::Value {
    crate::mcp::memory::run_memory_put(state, put_params(key, value, embed))
        .await
        .unwrap()
        .structured_content
        .unwrap()
}

fn mark_of(state: &ServerState) -> Option<Backfill> {
    backfill_marks(state).get(&(vis(), String::new())).copied()
}

/// A note is saved again while the backfill is embedding the text it listed. The row the backfill
/// then writes is of the earlier write: it must not replace the row the save wrote, and the pass
/// must not call the namespace level over it.
#[tokio::test]
async fn a_note_saved_again_while_its_backfill_embeds_keeps_its_new_row_and_is_not_called_level() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "k", "the old text").await;
    test_hooks::once_while_embedding(
        &state.shared.scope,
        Box::new(|state| {
            Box::pin(async move {
                let saved = put(state, "k", "the new text", true).await;
                assert_eq!(saved["embedded"], true, "{saved}");
            })
        }),
    );

    let during = search_json(&state, "the new text", 10).await;

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

    let after = search_json(&state, "the new text", 10).await;
    assert_eq!(hit_keys(&after), ["k"]);
    assert!(after.get("notice").is_none(), "{after}");
    let rows = rows_of(&state).await;
    assert!(rows.duplicated.is_empty(), "the row of the earlier write is gone");
    let note = crate::mcp::memory::memory_get_wire(&state, vis(), "", "k")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rows.stamps["k"], note.updated_at);
}

/// Level is what this process last saw. Another process may have saved a note with no row since,
/// so the belief lapses and a search looks again.
#[tokio::test]
async fn a_level_mark_lapses_and_the_next_search_reconciles_again() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "first", "a note").await;
    search_json(&state, "a note", 10).await;
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));
    // Saved outside this process's save path, as by another process whose embedding failed.
    save(&state, "second", "a note too").await;

    assert_eq!(
        hit_keys(&search_json(&state, "a note", 10).await),
        ["first"],
        "believed level: no listing, so the rowless note is not seen"
    );
    let owed = rows_owed_here(&state).await;
    backfill_marks(&state).insert(
        (vis(), String::new()),
        Backfill::Level {
            until: Instant::now(),
            ttl: LEVEL_TTL,
            owed,
        },
    );

    assert_eq!(hit_keys(&search_json(&state, "a note", 10).await), ["first", "second"]);
}

/// Another process saved a note it could not embed, or one saved with `embed: false`, while this
/// one believes the namespace level. Its save says so in the group's stamp, and the next search
/// here embeds the note without waiting for the belief to lapse.
#[tokio::test]
async fn a_rowless_save_in_another_process_ends_level_here() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "first", "a note").await;
    search_json(&state, "a note", 10).await;
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));

    // What another process's save does when it writes no row: the note, then the stamp.
    save(&state, "second", "a note too").await;
    owe_rows(root.path());

    assert_eq!(hit_keys(&search_json(&state, "a note", 10).await), ["first", "second"]);
    let stamp = rows_owed_here(&state).await;
    assert!(
        matches!(mark_of(&state), Some(Backfill::Level { owed, .. }) if owed == stamp),
        "level again, as of the stamp it read"
    );
}

/// The stamp other processes read changes with every save that wrote no row, and only with those.
#[tokio::test]
async fn a_save_without_a_row_rewrites_the_group_stamp() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    let untouched = rows_owed_here(&state).await;
    put(&state, "embedded", "a note", true).await;
    assert_eq!(
        rows_owed_here(&state).await,
        untouched,
        "a save with its row owes nothing"
    );

    put(&state, "deferred", "a note", false).await;
    let once = rows_owed_here(&state).await;
    assert_ne!(once, untouched);
    put(&state, "refused", "unembeddable text", true).await;
    assert_ne!(rows_owed_here(&state).await, once, "every rowless save is a new stamp");
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
#[tokio::test]
async fn a_save_with_its_row_in_one_preset_ends_level_in_another() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = server(root.path(), "model-a");
    put(&elsewhere, "first", "a note", true).await;
    search_json(&elsewhere, "a note", 10).await;
    let level = mark_of(&elsewhere).expect("a mark");
    assert!(matches!(level, Backfill::Level { .. }));
    drop(elsewhere);

    let saver = server(root.path(), "model-b");
    let own = rows_owed_here(&saver).await;
    assert_eq!(put(&saver, "second", "a note too", true).await["embedded"], true);
    assert_eq!(rows_owed_here(&saver).await, own, "the saver's own table owes nothing");
    drop(saver);

    let elsewhere = server(root.path(), "model-a");
    backfill_marks(&elsewhere).insert((vis(), String::new()), level);
    assert_eq!(
        hit_keys(&search_json(&elsewhere, "a note", 10).await),
        ["first", "second"]
    );
}

/// A save that wrote no row leaves a note the rows are behind: the embedding failed, or the save
/// asked for none. Either ends the belief that the namespace is level, and the next search by
/// meaning embeds the note.
#[tokio::test]
async fn a_save_that_wrote_no_row_ends_level_and_the_next_search_embeds_the_note() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    put(&state, "seed", "a seed note", true).await;
    search_json(&state, "a note", 10).await;
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));

    let deferred = put(&state, "later", "a note for later", false).await;
    assert_eq!(deferred["embedded"], false);
    assert_eq!(mark_of(&state), None, "a save without a row ends level");
    assert_eq!(hit_keys(&search_json(&state, "a note", 10).await), ["later", "seed"]);
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })));

    let failed = put(&state, "refused", "unembeddable text", true).await;
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
#[tokio::test]
async fn the_row_of_a_note_emptied_of_its_text_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    put(&state, "here", "some text", true).await;
    put(&state, "elsewhere", "some text", true).await;
    assert_eq!(rows_of(&state).await.stamps.len(), 2);

    put(&state, "here", "", false).await;
    assert_eq!(
        rows_of(&state).await.stamps.keys().collect::<Vec<_>>(),
        ["elsewhere"],
        "the save that emptied the note removed its row"
    );

    save(&state, "elsewhere", "").await;
    backfill_marks(&state).clear();
    search_json(&state, "some text", 10).await;
    assert!(rows_of(&state).await.stamps.is_empty(), "the backfill prunes it");
    assert!(matches!(mark_of(&state), Some(Backfill::Level { .. })), "and is level");
}

/// A join over more notes than one request may carry is read in several requests.
#[tokio::test]
async fn notes_beyond_one_request_are_read_in_chunks() {
    use crate::mcp::memory_ops::GET_MANY_MAX_KEYS;

    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "present", "a note").await;
    let mut keys: Vec<String> = (0..GET_MANY_MAX_KEYS * 2 + 3)
        .map(|index| format!("absent-{index}"))
        .collect();
    keys.push("present".to_string());

    let read = memory_get_many_wire(&state, vis(), "", keys.clone()).await.unwrap();

    assert_eq!(read.len(), keys.len(), "one slot per key, in order");
    assert_eq!(
        read.last().unwrap().as_ref().map(|note| note.key.as_str()),
        Some("present")
    );
    assert!(read[..keys.len() - 1].iter().all(Option::is_none));
}

/// Notes longer together than one reply may carry are read over several replies, each starting
/// at the first key the one before left unanswered.
#[tokio::test]
async fn notes_beyond_one_reply_are_read_in_several() {
    use crate::mcp::memory_ops::{GET_MANY_MAX_BYTES, NOTE_MAX_BYTES};

    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    // The longest note a save takes, with its one-byte tag: two of them, with their keys and
    // framing, pass one reply. ~keep
    let long = "x".repeat(NOTE_MAX_BYTES - 1);
    const { assert!(2 * NOTE_MAX_BYTES >= GET_MANY_MAX_BYTES) };
    let keys: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
    for key in &keys {
        save(&state, key, &long).await;
    }

    let read = memory_get_many_wire(&state, vis(), "", keys.clone()).await.unwrap();

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
    let joined = truth_join(decidable(&hits, &stamps, &notes), &notes, None, 10);
    assert_eq!(joined.hits.len(), 1);
    assert_eq!(
        joined.dropped, 3,
        "the stale row, the second row of a, the row of a gone note"
    );
}

/// A save and a delete end in an fsync, which must not run on the reactor: both go through
/// `blocking`. On this current-thread runtime that runs inline, on this thread.
#[tokio::test]
async fn a_save_and_a_delete_fsync_through_blocking() {
    use crate::mcp::index_route::blocking_calls;

    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");

    let before = blocking_calls();
    put(&state, "k", "a note", false).await;
    let after_save = blocking_calls();
    assert_eq!(
        after_save,
        before + 1,
        "the save's write and fsync run inside `blocking`"
    );

    let params = MemoryDeleteParams {
        key: "k".into(),
        visibility: Visibility::Group,
    };
    run_memory_delete(&state, params).await.unwrap();
    assert_eq!(blocking_calls(), after_save + 1, "and so do the delete's");
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
#[tokio::test]
async fn a_lapsed_look_that_finds_nothing_doubles_the_wait() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "k", "a note").await;
    search_json(&state, "note", 5).await;
    assert!(matches!(mark_of(&state), Some(Backfill::Level { ttl, .. }) if ttl == LEVEL_TTL));
    let owed = rows_owed_here(&state).await;
    backfill_marks(&state).insert(
        (vis(), String::new()),
        Backfill::Level {
            until: Instant::now(),
            ttl: LEVEL_TTL,
            owed,
        },
    );

    search_json(&state, "note", 5).await;

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
#[tokio::test]
async fn a_store_that_would_not_open_is_unavailable_at_once_to_the_next_call() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "k", "a note").await;
    let dir = memory_lance_dir(root.path(), "fake", DIM);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();
    let params = || MemorySearchParams {
        query: "a note".into(),
        limit: Some(5),
        tag: None,
        visibility: Visibility::Group,
    };
    let first = run_memory_search(&state, params()).await.unwrap_err();
    assert!(
        first.message.starts_with("`memory_search`: vector store unavailable"),
        "{}",
        first.message
    );
    assert_eq!(
        first.code,
        rmcp::model::ErrorCode::INVALID_REQUEST,
        "a refusal, not an internal error"
    );

    std::fs::remove_file(&dir).unwrap();
    let second = run_memory_search(&state, params()).await.unwrap_err();
    assert!(second.message.contains("unavailable"), "{}", second.message);
    assert!(!dir.exists(), "the store was not opened again");
}
