use super::*;
use crate::extract::doc::{DocChunk, DocEntity, DocKeyword};
use crate::store::DocEntry;

fn blob(text: &str, vectors: bool) -> FileMapDoc {
    FileMapDoc {
        config_digest: "digest".to_string(),
        linear_chunked: false,
        text_fallback: false,
        embed_failed: false,
        schema_ver: crate::extract::SCHEMA_VER,
        mime_type: "text/markdown".to_string(),
        content: format!("{text} one\n{text} two\n"),
        metadata: vec![("title".to_string(), format!("{text} title"))],
        detected_languages: Vec::new(),
        chunks: vec![
            DocChunk {
                byte_start: 0,
                byte_end: 8,
                text: format!("{text} one"),
                embedding: if vectors { vec![0.5; 768] } else { Vec::new() },
                section: vec!["Top".to_string(), "Sub".to_string()],
                line_start: Some(1),
                line_end: Some(1),
                ..DocChunk::default()
            },
            DocChunk {
                byte_start: 8,
                byte_end: 16,
                text: format!("{text} two"),
                embedding: if vectors { vec![0.5; 768] } else { Vec::new() },
                ..DocChunk::default()
            },
        ],
        embedding_model: if vectors {
            crate::embeddings::model_key("balanced").into_owned()
        } else {
            String::new()
        },
        embedding_dim: if vectors { 768 } else { 0 },
        keywords: vec![DocKeyword {
            text: "Flood Barrier".to_string(),
            score: 0.1,
            algorithm: "yake".to_string(),
        }],
        entities: vec![DocEntity {
            category: "LOCATION".to_string(),
            text: "x".to_string(),
            start: 9,
            end: 10,
            confidence: None,
        }],
        summary: None,
        language_confidences: Vec::new(),
    }
}

/// Write `doc`'s blob and track it at each of `paths`; returns its content hash. Blobs are global to
/// the test process, so tests that run at once must not write one content with and without
/// vectors: each test uses its own texts unless it reads the same blob only.
fn track(store: &mut Store, doc: &FileMapDoc, paths: &[&str]) -> String {
    let hash = crate::hashing::hash_bytes(doc.content.as_bytes());
    store.write_doc(&hash, doc).unwrap();
    let hex = crate::hashing::hex(&hash);
    for path in paths {
        store.upsert_doc(
            *path,
            DocEntry {
                config_digest: "digest".to_string(),
                hash_hex: hex.clone(),
                embedding_preset: "balanced".to_string(),
                size_bytes: 1,
                mtime: 0,
                embedded: doc.embedding_dim > 0,
                embed_attempted: true,
            },
        );
    }
    hex
}

fn setup() -> (tempfile::TempDir, Store, Config, Filters) {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
    let mut config = crate::config::default_for_root(std::path::Path::new("."));
    config.documents.embed = true;
    let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
    (tmp, store, config, filters)
}

fn doc_store(store: &Store, config: &Config) -> DocStore {
    open_doc_store(store, config, false).unwrap().expect("store exists")
}

#[test]
fn rows_carry_citations_entities_keywords_and_title() {
    let rows = build_doc_rows(blob("alpha", true), "k".to_string(), true);
    assert_eq!(rows.mime_group, "markdown");
    assert_eq!(rows.title.as_deref(), Some("alpha title"));
    assert_eq!(rows.keywords, vec!["flood barrier".to_string()]);
    assert_eq!(rows.chunks[0].section.as_deref(), Some("Top > Sub"));
    assert_eq!(rows.chunks[0].line_start, Some(1));
    assert!(rows.chunks[0].entity_cats.is_empty());
    assert_eq!(rows.chunks[1].entity_cats, vec!["location".to_string()]);
    assert!(rows.embedded());
    assert!(!build_doc_rows(blob("alpha", true), "k".to_string(), false).embedded());
}

/// The chunk cap bounds embedding, not search: a document over it keeps its chunk rows, so the
/// lexical lane finds it, and a blob embedded before the cap applied to its chunker keeps its
/// vectors instead of being rewritten without them on every scan (review 3 M1).
#[test]
fn a_document_over_the_chunk_cap_keeps_its_chunk_rows() {
    let (_tmp, mut store, mut config, filters) = setup();
    config.documents.max_chunks_per_document = 1;
    track(&mut store, &blob("overcap bare", false), &["bare.md"]);
    track(&mut store, &blob("overcap embedded", true), &["embedded.md"]);
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        2
    );
    let caps = doc_store(&store, &config).capabilities().unwrap();
    assert_eq!((caps.rows, caps.vectors), (4, 2));
    assert!(caps.lexical);
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        0,
        "nothing is rewritten"
    );
}

/// A blob is keyed by content alone, so another workspace with other document settings or another
/// model overwrites it. Rows are never built from such a blob: its entry is dropped and the path
/// owed, so the next pass extracts the file again under this view's settings (review 3 S1).
#[test]
fn a_blob_of_other_settings_or_another_model_is_extracted_again_not_written() {
    let (_tmp, mut store, config, filters) = setup();
    let mine = blob("displaced digest", false);
    let hash = crate::hashing::hash_bytes(mine.content.as_bytes());
    track(&mut store, &mine, &["digest.md"]);
    let mut theirs = mine.clone();
    theirs.config_digest = "other settings".to_string();
    store.write_doc(&hash, &theirs).unwrap();

    let embedded = blob("displaced model", true);
    let hash = crate::hashing::hash_bytes(embedded.content.as_bytes());
    track(&mut store, &embedded, &["model.md"]);
    let mut other_model = embedded.clone();
    other_model.embedding_model = "another-model".to_string();
    store.write_doc(&hash, &other_model).unwrap();

    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        0,
        "nothing is built from another workspace's blob"
    );
    for path in ["digest.md", "model.md"] {
        assert!(store.lookup_doc(path).is_none(), "{path} is extracted again");
        assert!(
            store.pending_vectors.docs().contains(bstr::BStr::new(path.as_bytes())),
            "{path} owes its rows"
        );
    }
}

/// An incremental pass that touched no document, dropped none and owes none leaves the store alone:
/// it does not look for absent rows, list keys or run index upkeep (review 3 S4). A pass that owes
/// rows still writes them.
#[test]
fn an_incremental_pass_without_document_work_leaves_the_store_alone() {
    let (_tmp, mut store, config, filters) = setup();
    let key = doc_key(&track(&mut store, &blob("idle", false), &["a.md"]), "digest");
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    doc_store(&store, &config)
        .delete_keys(std::slice::from_ref(&key))
        .unwrap();
    let incremental = crate::scanner_flush::REQUEUE_PER_INCREMENTAL;
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), incremental),
        0,
        "the pass looked for absent rows"
    );
    store.pending_vectors.owe_doc("a.md");
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), incremental),
        1,
        "a pass that owes rows writes them"
    );
}

#[test]
fn a_scan_writes_absent_documents_once_per_content_and_settles_their_paths() {
    let (_tmp, mut store, config, filters) = setup();
    track(&mut store, &blob("alpha", true), &["a.md", "copy/a.md"]);
    track(&mut store, &blob("beta", false), &["b.md"]);
    store.pending_vectors.owe_doc("a.md");
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        2,
        "two contents, three paths"
    );
    assert!(store.pending_vectors.docs().is_empty());
    let caps = doc_store(&store, &config).capabilities().unwrap();
    assert_eq!((caps.rows, caps.vectors), (4, 2));
    assert!(caps.lexical, "the scan builds the full-text index");
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        0,
        "nothing absent, nothing written"
    );
}

#[test]
fn a_wiped_store_is_rebuilt_from_the_blobs() {
    let (_tmp, mut store, config, filters) = setup();
    track(&mut store, &blob("wiped", true), &["a.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    let dir = doc_store(&store, &config).dir().to_path_buf();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        1
    );
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 2);
}

#[test]
fn rows_without_vectors_are_replaced_once_the_document_is_embedded() {
    let (_tmp, mut store, config, filters) = setup();
    let bare = blob("bare", false);
    let hash = crate::hashing::hash_bytes(bare.content.as_bytes());
    track(&mut store, &bare, &["a.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 0);
    // The same content, now embedded: same key, the rows gain their vectors.
    track(&mut store, &blob("bare", true), &["a.md"]);
    assert_eq!(crate::hashing::hash_bytes(blob("bare", true).content.as_bytes()), hash);
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        1
    );
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 2);
}

#[test]
fn rows_no_document_names_are_deleted_and_removal_never_creates_a_store() {
    let (_tmp, mut store, config, filters) = setup();
    remove_dead_document_rows(&mut store, &config);
    assert!(open_doc_store(&store, &config, false).unwrap().is_none());
    track(&mut store, &blob("alpha", true), &["a.md"]);
    track(&mut store, &blob("beta", true), &["b.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    store.remove_doc("b.md");
    remove_dead_document_rows(&mut store, &config);
    assert_eq!(doc_store(&store, &config).keys().unwrap().len(), 1);
}

#[test]
fn excluded_documents_lose_their_vectors_and_stay_searchable() {
    let (_tmp, mut store, mut config, _) = setup();
    track(&mut store, &blob("alpha", true), &["docs/a.md"]);
    track(&mut store, &blob("beta", true), &["notes/b.md"]);
    let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    config.documents.embed_exclude = vec!["docs/**".to_string()];
    let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
    assert!(strip_unwanted_vectors(&store, &config, &filters));
    let caps = doc_store(&store, &config).capabilities().unwrap();
    assert_eq!((caps.rows, caps.vectors), (4, 2));
    config.documents.embed = false;
    let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
    assert!(strip_unwanted_vectors(&store, &config, &filters));
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 0);
}

/// Review M2: a document whose new version has no rows (zero chunks, over the chunk cap, excluded)
/// must not keep serving the old version's rows. Rows are keyed by content, so the old version's key
/// is dead once the path names the new content.
#[test]
fn a_new_version_without_rows_drops_the_old_versions_rows() {
    let (_tmp, mut store, config, filters) = setup();
    track(&mut store, &blob("version one", true), &["guide.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().rows, 2);

    let mut empty = blob("version two", false);
    empty.chunks.clear();
    track(&mut store, &empty, &["guide.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    remove_dead_document_rows(&mut store, &config);

    assert_eq!(
        doc_store(&store, &config).capabilities().unwrap().rows,
        0,
        "no row of the old version is served"
    );
}

/// Review M8: a document under an `[scan] extra_roots` root is keyed by its absolute path. It used
/// to be written under its own scope, which the search never asked for; rows now carry no scope and
/// the view's document map, external keys included, is what the search allows.
#[test]
fn an_external_root_document_is_written_and_allowed_like_any_other() {
    let (_tmp, mut store, config, filters) = setup();
    let key = track(&mut store, &blob("external spec", true), &["/data/specs/retry.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);

    let live = crate::lance::docs_rank::LiveDocs::of_store(&store, None);
    let doc_key = crate::lance::docs::doc_key(&key, "digest");
    assert_eq!(
        live.by_key.get(&doc_key),
        Some(&vec!["/data/specs/retry.md".to_string()])
    );
    assert_eq!(
        doc_store(&store, &config).keys().unwrap().len(),
        1,
        "its rows are written"
    );
}

/// A blob torn by a power loss still exists, so its document would be skipped as unchanged for
/// good and its rows never written. Its entry is dropped (the next pass extracts the file again)
/// and its rows stay owed.
#[test]
fn a_torn_blob_sends_its_document_to_be_extracted_again() {
    let (_tmp, mut store, config, filters) = setup();
    let hex = track(&mut store, &blob("torn blob", true), &["torn.md"]);
    let path = store.blob_path_doc_hex(&hex);
    let whole = std::fs::read(&path).unwrap();
    std::fs::write(&path, &whole[..whole.len() / 2]).unwrap();

    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);

    assert!(store.lookup_doc("torn.md").is_none(), "the entry is dropped");
    let owed: Vec<String> = store
        .pending_vectors
        .docs()
        .iter()
        .map(|rel| rel.to_str_lossy().into_owned())
        .collect();
    assert_eq!(owed, ["torn.md"], "its rows stay owed");
}

fn git(args: &[&str], cwd: &std::path::Path) {
    let out = crate::testing::git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Two worktrees of one repository: their stores are views of one group.
fn worktrees() -> (tempfile::TempDir, Store, Store, Config, Filters) {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git(&["init", "-q", "-b", "main"], &main);
    git(
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
        &main,
    );
    let linked = tmp.path().join("linked");
    git(
        &["worktree", "add", "-q", linked.to_str().unwrap(), "-b", "linked"],
        &main,
    );
    let a = Store::open(&main, crate::store::VIEW_WORKING).unwrap();
    let b = Store::open(&linked, crate::store::VIEW_WORKING).unwrap();
    let mut config = crate::config::default_for_root(std::path::Path::new("."));
    config.documents.embed = true;
    let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
    (tmp, a, b, config, filters)
}

#[test]
fn worktrees_of_a_group_share_one_document_store_and_sweep_only_what_no_view_names() {
    let (_tmp, mut a, mut b, config, filters) = worktrees();
    let shared = blob("group-shared", true);
    let key = doc_key(&track(&mut a, &shared, &["guide.md"]), "digest");
    track(&mut b, &shared, &["guide.md"]);
    track(&mut b, &blob("group-only-b", true), &["b.md"]);
    let written_a = sync_document_rows(&mut a, &config, &filters, &AHashSet::new(), usize::MAX);
    a.flush().unwrap();
    let written_b = sync_document_rows(&mut b, &config, &filters, &AHashSet::new(), usize::MAX);
    b.flush().unwrap();

    assert_eq!(
        doc_store(&a, &config).dir(),
        doc_store(&b, &config).dir(),
        "one store for the group"
    );
    assert_eq!((written_a, written_b), (1, 1), "the shared document is written once");
    let store = doc_store(&a, &config);
    assert_eq!(store.keys().unwrap().len(), 2);

    // Removed in one worktree, the document keeps its rows for the other.
    a.remove_doc("guide.md");
    remove_dead_document_rows(&mut a, &config);
    a.flush().unwrap();
    assert!(store.keys().unwrap().contains(&key), "b still names it");

    // Removed in both, its rows are swept.
    b.remove_doc("guide.md");
    remove_dead_document_rows(&mut b, &config);
    let keys = store.keys().unwrap();
    assert!(!keys.contains(&key), "no view names it");
    assert_eq!(keys.len(), 1, "b's other document stays");
}

#[test]
fn an_unreadable_sibling_document_map_deletes_nothing() {
    let (_tmp, mut a, mut b, config, filters) = worktrees();
    track(&mut a, &blob("group-unknown-a", true), &["a.md"]);
    track(&mut b, &blob("group-unknown-b", true), &["b.md"]);
    sync_document_rows(&mut a, &config, &filters, &AHashSet::new(), usize::MAX);
    a.flush().unwrap();
    sync_document_rows(&mut b, &config, &filters, &AHashSet::new(), usize::MAX);
    std::fs::write(b.view_dir.join(crate::store::INDEX_FILE), b"\xc1 not msgpack").unwrap();

    a.remove_doc("a.md");
    remove_dead_document_rows(&mut a, &config);

    assert_eq!(
        doc_store(&a, &config).keys().unwrap().len(),
        2,
        "liveness unknown: nothing is deleted"
    );
}

/// A sibling's scan names a document in its file map, then writes the document's rows. A sweep that
/// read the group's liveness before that map was written must not delete those rows: the write
/// waits for the sweep, or the sweep sees the key.
#[test]
fn a_sweep_never_deletes_rows_a_sibling_writes_after_liveness_was_read() {
    let (_tmp, mut a, mut b, config, filters) = worktrees();
    track(&mut a, &blob("race-own", true), &["a.md"]);
    sync_document_rows(&mut a, &config, &filters, &AHashSet::new(), usize::MAX);
    a.flush().unwrap();
    let store = doc_store(&a, &config);
    let key = doc_key(&track(&mut b, &blob("race-sibling", true), &["b.md"]), "digest");

    let (go, wait_go) = std::sync::mpsc::channel::<()>();
    let (written, wait_written) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let (b, config, sibling_key) = (&mut b, &config, key.clone());
        scope.spawn(move || {
            wait_go.recv().unwrap();
            // A scan writes its file map before its rows (`flush_code_map`, then the lanes).
            b.flush().unwrap();
            let rows = build_doc_rows(blob("race-sibling", true), sibling_key, true);
            open_doc_store(b, config, true)
                .unwrap()
                .unwrap()
                .write(&[rows])
                .unwrap();
            let _ = written.send(());
        });
        sweep_dead_rows_as_of(&store, || {
            let live = a.group_live_doc_keys();
            go.send(()).unwrap();
            // Long enough for an ungated write to land before the delete.
            let _ = wait_written.recv_timeout(std::time::Duration::from_secs(3));
            live
        });
    });

    assert!(
        store.keys().unwrap().contains(&key),
        "the sibling's rows survive the sweep"
    );
}

/// A view whose embed policy excludes a document strips the vectors only of rows no other view of
/// the group names: a sibling that wants them keeps them.
#[test]
fn stripping_vectors_leaves_the_rows_a_sibling_names() {
    let (_tmp, mut a, mut b, mut config, filters) = worktrees();
    let shared = blob("strip-shared", true);
    let shared_key = doc_key(&track(&mut a, &shared, &["docs/shared.md"]), "digest");
    track(&mut b, &shared, &["docs/shared.md"]);
    let own_key = doc_key(&track(&mut a, &blob("strip-own", true), &["docs/own.md"]), "digest");
    sync_document_rows(&mut a, &config, &filters, &AHashSet::new(), usize::MAX);
    a.flush().unwrap();
    b.flush().unwrap();

    config.documents.embed_exclude = vec!["docs/**".to_string()];
    let excluding = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
    assert!(strip_unwanted_vectors(&a, &config, &excluding));

    let present = doc_store(&a, &config)
        .present(&[shared_key.clone(), own_key.clone()])
        .unwrap();
    assert_eq!(present.get(&shared_key), Some(&true), "b names it: its vectors stay");
    assert_eq!(present.get(&own_key), Some(&false), "only a names it: stripped");
}

/// An incremental pass deletes only the rows of the keys it dropped; a dead row it did not drop
/// (left by a pass that died before its sweep) waits for the next full pass, which sweeps it.
#[test]
fn an_incremental_pass_sweeps_only_the_keys_it_dropped() {
    let (_tmp, mut store, config, filters) = setup();
    track(&mut store, &blob("sweep-kept", true), &["kept.md"]);
    let orphan = doc_key(
        &track(&mut store, &blob("sweep-orphan", true), &["orphan.md"]),
        "digest",
    );
    let dropped = doc_key(
        &track(&mut store, &blob("sweep-dropped", true), &["dropped.md"]),
        "digest",
    );
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    // A pass that died after dropping the entry, before its sweep: the store keeps the rows.
    store.remove_doc("orphan.md");
    store.dropped_doc_keys.clear();

    store.remove_doc("dropped.md");
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), 64);
    let keys = doc_store(&store, &config).keys().unwrap();
    assert!(!keys.contains(&dropped), "the key this pass dropped is swept");
    assert!(keys.contains(&orphan), "an incremental pass checks no other key");

    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    let keys = doc_store(&store, &config).keys().unwrap();
    assert!(!keys.contains(&orphan), "a full pass sweeps every dead key");
    assert_eq!(keys.len(), 1);
}

/// A store an earlier build kept under `views/<view>/retrieval/` is removed by the next scan, and
/// the group store is filled from the cached blobs: the rows carry the blob's vectors, and no
/// embedder is reached (the sync has none). A view with no documents removes its old store too.
#[test]
fn the_per_view_store_of_an_earlier_build_is_replaced_by_the_group_store() {
    let (_tmp, mut store, config, filters) = setup();
    let legacy = store.view_dir.join(crate::store_layout::RETRIEVAL_DIR);
    let seed = |legacy: &std::path::Path| {
        std::fs::create_dir_all(legacy.join("balanced-768-0000")).unwrap();
        std::fs::write(legacy.join("balanced-768-0000").join("rows"), b"old rows").unwrap();
    };
    seed(&legacy);
    track(&mut store, &blob("legacy migrated", true), &["a.md"]);

    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        1
    );
    assert!(!legacy.exists(), "the per-view store is removed");
    let caps = doc_store(&store, &config).capabilities().unwrap();
    assert_eq!((caps.rows, caps.vectors), (2, 2), "rows and vectors come from the blob");

    let (_tmp, mut empty, config, filters) = setup();
    let legacy = empty.view_dir.join(crate::store_layout::RETRIEVAL_DIR);
    seed(&legacy);
    sync_document_rows(&mut empty, &config, &filters, &AHashSet::new(), usize::MAX);
    assert!(!legacy.exists(), "a view without documents removes it too");
}

/// A scan's open stamps its own store and removes a sibling store of another model no scan has
/// opened for the idle window, judged by the stamp or, before it, the directory's mtime.
#[test]
fn an_open_removes_retrieval_stores_of_models_no_scan_uses() {
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("lance");
    let now = std::time::SystemTime::now();
    let old = now - IDLE_MODEL_STORE - std::time::Duration::from_secs(60);
    let mk = |name: &str| {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), b"{}").unwrap();
        dir
    };
    let current = mk("balanced-768-aaaaaaaa");
    let stamped_old = mk("small-384-bbbbbbbb");
    std::fs::File::create(stamped_old.join(LAST_USED_FILE))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let unstamped_old = mk("large-1024-cccccccc");
    std::fs::File::open(&unstamped_old).unwrap().set_modified(old).unwrap();
    let recent = mk("other-768-dddddddd");
    std::fs::File::create(recent.join(LAST_USED_FILE)).unwrap();

    prune_idle_model_stores(&current, now);

    assert!(current.join(LAST_USED_FILE).exists(), "the open store is stamped");
    assert!(!stamped_old.exists());
    assert!(!unstamped_old.exists());
    assert!(recent.exists(), "a store another worktree used lately stays");
}
