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

fn hit(key: &str, value: &str, updated_at: i64) -> MemoryHit {
    MemoryHit {
        key: key.into(),
        value: value.into(),
        tags: vec!["row".into()],
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
        value: value.into(),
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

    assert_eq!(dir.file_name().unwrap(), "balanced-v2-384");
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
