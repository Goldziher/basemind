use super::*;

#[test]
fn preset_dim_for_balanced_returns_768() {
    let dim = preset_dim("balanced").expect("balanced preset");
    assert_eq!(dim, 768);
}

/// A cached doc embedded under `balanced` (dim 768) must NOT be reused when the configured
/// preset switches to `multilingual` (also dim 768, different model) — the dim matches, so only
/// the model check closes the stale-vector hole. It stays reusable when the preset is unchanged.
#[test]
fn cached_doc_not_reusable_when_preset_model_differs_at_same_dim() {
    use crate::extract::doc::{DocChunk, FileMapDoc};
    let doc = FileMapDoc {
        config_digest: String::new(),
        schema_ver: 0,
        mime_type: "application/pdf".to_string(),
        content: "hello".to_string(),
        metadata: Vec::new(),
        detected_languages: Vec::new(),
        chunks: vec![DocChunk {
            byte_start: 0,
            byte_end: 5,
            text: "hello".to_string(),
            embedding: vec![0.0_f32; 768],
        }],
        embedding_model: "balanced".to_string(),
        embedding_dim: 768,
        keywords: Vec::new(),
        entities: Vec::new(),
        summary: None,
        language_confidences: Vec::new(),
    };

    let same = DocumentsConfig {
        embedding_preset: "balanced".to_string(),
        ..DocumentsConfig::default()
    };
    assert!(
        cached_doc_is_reusable(&doc, &same, true, "d"),
        "same preset (balanced) must reuse the cached vectors"
    );

    let switched = DocumentsConfig {
        embedding_preset: "multilingual".to_string(),
        ..DocumentsConfig::default()
    };
    assert!(
        !cached_doc_is_reusable(&doc, &switched, true, "d"),
        "switching balanced -> multilingual (same dim, different model) must force recompute"
    );

    assert!(
        cached_doc_is_reusable(&doc, &switched, false, "d"),
        "embedding off: cached doc is always reusable"
    );
}

/// Structural regression guard for the streaming-flush memory fix: the accumulated per-file
/// descriptor must stay metadata-only. This exhaustive struct literal names EXACTLY the metadata
/// fields — re-introducing a `rows: Vec<DocumentRow>` (or any embedding payload) field would
/// break this compile, catching a regression back to the corpus-wide accumulation that held
/// multiple GB resident on a large repo.
#[test]
fn pending_doc_batch_is_metadata_only() {
    let batch = PendingDocBatch {
        rel_path: "docs/manual.pdf".to_string(),
        blob_hash: "deadbeef".to_string(),
        doc_scope: "repo:origin".to_string(),
        chunk_count: 3,
        embedding_dim: 768,
        emit_rows: true,
        embedded: true,
        embed_attempted: true,
        reused: false,
    };
    assert!(batch.emit_rows);
    assert_eq!(batch.chunk_count, 3);
}

/// Build a minimal document fixture: `chunk_count` chunks, each carrying an `embedding_dim`-long
/// vector (dim `0` leaves the chunks vectorless — the failed-embed shape).
fn doc_fixture(chunk_count: usize, embedding_dim: u16) -> crate::extract::doc::FileMapDoc {
    use crate::extract::doc::{DocChunk, FileMapDoc};
    let chunks = (0..chunk_count)
        .map(|i| DocChunk {
            byte_start: i as u32,
            byte_end: i as u32 + 1,
            text: format!("chunk {i}"),
            embedding: vec![0.0_f32; embedding_dim as usize],
        })
        .collect();
    FileMapDoc {
        config_digest: String::new(),
        schema_ver: 0,
        mime_type: "application/pdf".to_string(),
        content: "body".to_string(),
        metadata: Vec::new(),
        detected_languages: Vec::new(),
        chunks,
        embedding_model: if embedding_dim > 0 {
            "balanced".to_string()
        } else {
            String::new()
        },
        embedding_dim,
        keywords: Vec::new(),
        entities: Vec::new(),
        summary: None,
        language_confidences: Vec::new(),
    }
}

/// GAP-2 regression (#44 follow-up): a fresh extraction that asked to embed but got zero vectors
/// back (an unembeddable body / missing ONNX) is recorded as *attempted* — so the fast path can
/// stop re-extracting it every scan — while `embedded` stays false because no vectors exist. A
/// successful embed sets both; an embed-off pass sets neither attempt nor a false `embedded`.
#[test]
fn pending_from_doc_marks_a_failed_embed_as_attempted_not_embedded() {
    let cfg = DocumentsConfig::default();

    let failed = doc_fixture(3, 0);
    let batch = pending_from_doc(&failed, "docs/a.pdf", "hash", "repo:x", &cfg, true, false);
    assert!(!batch.embedded, "no vectors => not embedded");
    assert!(
        batch.embed_attempted,
        "a fresh embed-requested extraction counts as an attempt"
    );
    assert!(!batch.emit_rows, "a vectorless doc emits no LanceDB rows");

    let ok = doc_fixture(3, 768);
    let batch = pending_from_doc(&ok, "docs/a.pdf", "hash", "repo:x", &cfg, true, false);
    assert!(
        batch.embedded && batch.embed_attempted,
        "a successful embed sets both flags"
    );

    let off = pending_from_doc(&failed, "docs/a.pdf", "hash", "repo:x", &cfg, false, false);
    assert!(off.embedded, "embed off leaves the requirement trivially satisfied");
    assert!(!off.embed_attempted, "embed off ran no attempt");

    let reused = pending_from_doc(&ok, "docs/a.pdf", "hash", "repo:x", &cfg, true, true);
    assert!(
        !reused.embed_attempted,
        "a reused blob ran no fresh attempt (already embedded)"
    );
}

/// GAP-2 regression (#44 follow-up): the fast-path predicate must treat an already-attempted doc
/// as settled so a deterministically-unembeddable file stops thrashing, while a legacy entry
/// (attempt flag absent, deserialized `false`) still heals exactly once, and a content-hash or
/// preset change re-opens the attempt.
#[test]
fn doc_entry_settled_backs_off_after_a_failed_embed_attempt() {
    use crate::store::DocEntry;
    let base = DocEntry {
        config_digest: String::new(),
        hash_hex: "h".to_string(),
        embedding_preset: "balanced".to_string(),
        size_bytes: 10,
        mtime: 0,
        embedded: false,
        embed_attempted: true,
    };
    assert!(
        doc_entry_settled(&base, "h", "balanced", true, "", false),
        "attempted-but-vectorless: settled, do not re-extract every scan"
    );

    let legacy = DocEntry {
        config_digest: String::new(),
        embed_attempted: false,
        ..base.clone()
    };
    assert!(
        !doc_entry_settled(&legacy, "h", "balanced", true, "", false),
        "a pre-field entry heals exactly once (attempt flag defaults false)"
    );

    assert!(
        !doc_entry_settled(&base, "h2", "balanced", true, "", false),
        "a content-hash change re-opens the attempt"
    );
    assert!(
        !doc_entry_settled(&base, "h", "multilingual", true, "", false),
        "a preset change re-opens the attempt"
    );
    assert!(
        doc_entry_settled(&legacy, "h", "balanced", false, "", false),
        "no embed requested this pass: settled regardless of the attempt flag"
    );
}

fn filters_for(documents: DocumentsConfig) -> Filters {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents = documents;
    Filters::build(&config, Vec::new()).expect("build filters")
}

/// `doc_embed_requested` is the single gate both `extract_and_persist_doc` and the
/// `process_doc` unchanged fast path consult; this truth table is the contract that keeps the
/// two sides agreeing about "will this pass embed this rel" (a disagreement is the #44 loop).
#[test]
fn doc_embed_requested_truth_table() {
    let on = DocumentsConfig {
        embed: true,
        ..DocumentsConfig::default()
    };
    let on_filters = filters_for(on.clone());
    assert!(
        !doc_embed_requested("docs/a.pdf", &on, EmbedMode::Deferred, &on_filters),
        "Deferred pass never embeds"
    );
    assert!(
        doc_embed_requested("docs/a.pdf", &on, EmbedMode::Inline, &on_filters),
        "Inline + embed on must embed"
    );

    let off = DocumentsConfig {
        embed: false,
        ..DocumentsConfig::default()
    };
    assert!(
        !doc_embed_requested("docs/a.pdf", &off, EmbedMode::Inline, &filters_for(off.clone())),
        "Inline with embed off must not embed"
    );

    let excluded = DocumentsConfig {
        embed: true,
        embed_exclude: vec!["docs/**".to_string()],
        ..DocumentsConfig::default()
    };
    let excluded_filters = filters_for(excluded.clone());
    assert!(
        !doc_embed_requested("docs/a.pdf", &excluded, EmbedMode::Inline, &excluded_filters),
        "embed_exclude match must not embed"
    );
    assert!(
        doc_embed_requested("notes/a.pdf", &excluded, EmbedMode::Inline, &excluded_filters),
        "non-excluded path still embeds"
    );

    let included = DocumentsConfig {
        embed: true,
        embed_include: vec!["docs/**".to_string()],
        embed_exclude: vec!["docs/private/**".to_string()],
        ..DocumentsConfig::default()
    };
    let included_filters = filters_for(included.clone());
    assert!(doc_embed_requested(
        "docs/a.pdf",
        &included,
        EmbedMode::Inline,
        &included_filters
    ));
    assert!(
        !doc_embed_requested("notes/a.pdf", &included, EmbedMode::Inline, &included_filters),
        "outside embed_include must not embed"
    );
    assert!(
        !doc_embed_requested("docs/private/a.pdf", &included, EmbedMode::Inline, &included_filters),
        "embed_exclude beats embed_include"
    );
}

/// A changed extraction digest or a changed embed policy must defeat the unchanged fast path; a
/// legacy entry (empty digest) is re-checked once so its blob can be stamped.
#[test]
fn doc_entry_settled_requires_current_digest_and_honours_reflush() {
    use crate::store::DocEntry;
    let entry = DocEntry {
        config_digest: "d1".to_string(),
        hash_hex: "h".to_string(),
        embedding_preset: "balanced".to_string(),
        size_bytes: 10,
        mtime: 0,
        embedded: true,
        embed_attempted: true,
    };
    assert!(doc_entry_settled(&entry, "h", "balanced", true, "d1", false));
    assert!(
        !doc_entry_settled(&entry, "h", "balanced", true, "d2", false),
        "digest change re-processes"
    );
    assert!(
        !doc_entry_settled(&entry, "h", "balanced", false, "d2", false),
        "digest change re-processes even when nothing is embedded"
    );
    let legacy = DocEntry {
        config_digest: String::new(),
        ..entry.clone()
    };
    assert!(
        !doc_entry_settled(&legacy, "h", "balanced", true, "d1", false),
        "legacy entry is re-checked once"
    );
    assert!(
        !doc_entry_settled(&entry, "h", "balanced", true, "d1", true),
        "policy change re-flushes docs that want vectors"
    );
    assert!(
        doc_entry_settled(&entry, "h", "balanced", false, "d1", true),
        "policy change leaves docs that no longer want vectors alone"
    );
}

#[test]
fn cached_doc_is_reusable_rejects_a_foreign_digest_but_adopts_a_legacy_blob() {
    let mut doc = doc_fixture(2, 768);
    let cfg = DocumentsConfig::default();
    assert!(
        cached_doc_is_reusable(&doc, &cfg, false, "cur"),
        "legacy blob adoptable"
    );
    doc.config_digest = "cur".to_string();
    assert!(cached_doc_is_reusable(&doc, &cfg, false, "cur"));
    assert!(
        !cached_doc_is_reusable(&doc, &cfg, false, "other"),
        "blob built under different chunk settings is stale"
    );
}

#[test]
fn extension_denylist_accepts_leading_dot_and_any_case() {
    let cfg = DocumentsConfig {
        extension_denylist: vec![".PDF".to_string(), "Docx".to_string()],
        ..DocumentsConfig::default()
    };
    assert!(is_denied_binary_or_archive(
        Path::new("a/b.pdf"),
        "application/pdf",
        &cfg
    ));
    assert!(is_denied_binary_or_archive(
        Path::new("a/b.DOCX"),
        "application/msword",
        &cfg
    ));
    assert!(!is_denied_binary_or_archive(Path::new("a/b.md"), "text/markdown", &cfg));
}

#[test]
fn preset_dim_for_unknown_errors() {
    let err = preset_dim("does-not-exist").expect_err("unknown preset");
    let msg = err.to_string();
    assert!(
        msg.contains("does-not-exist"),
        "error should name the preset; got: {msg}"
    );
}

#[test]
fn extract_archives_toggle_gates_only_archives_not_binaries() {
    let default_cfg = DocumentsConfig::default();
    assert!(!default_cfg.extract_archives, "archives rejected by default");
    assert!(is_denied_binary_or_archive(
        Path::new("bundle.zip"),
        "application/zip",
        &default_cfg
    ));

    let extract_cfg = DocumentsConfig {
        extract_archives: true,
        ..DocumentsConfig::default()
    };
    assert!(
        !is_denied_binary_or_archive(Path::new("bundle.zip"), "application/zip", &extract_cfg),
        "extract_archives=true must route archives to the extractor"
    );
    assert!(
        is_denied_binary_or_archive(Path::new("libfoo.so"), "application/x-sharedlib", &extract_cfg),
        "binaries stay denied even with extract_archives=true"
    );
    assert!(
        is_denied_binary_or_archive(Path::new("mod.wasm"), "application/wasm", &extract_cfg),
        "wasm binary stays denied"
    );
}

#[test]
fn matches_mime_exact_and_prefix() {
    assert!(matches_mime("application/pdf", "application/pdf"));
    assert!(matches_mime("image/", "image/png"));
    assert!(matches_mime("image/", "image/jpeg"));
    assert!(!matches_mime("image/", "video/mp4"));
    assert!(!matches_mime("application/pdf", "application/json"));
    assert!(!matches_mime("image/", "imageprocessing/x"));
}

#[test]
fn doc_config_from_propagates_language_settings() {
    use crate::config::DocLanguageConfig;
    let cfg = DocumentsConfig {
        language: DocLanguageConfig {
            auto_detect: true,
            min_confidence: 0.5,
            detect_multiple: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let doc_cfg = doc_config_from(&cfg, &LlmConfig::default(), &ResourcesConfig::default(), cfg.embed);
    assert!(doc_cfg.language.auto_detect);
    assert_eq!(doc_cfg.language.min_confidence, 0.5);
    assert!(doc_cfg.language.detect_multiple);
}

#[test]
fn doc_config_from_propagates_extraction_limits() {
    let cfg = DocumentsConfig {
        max_pages: 37,
        extraction_timeout_secs: 42,
        ..Default::default()
    };
    let doc_cfg = doc_config_from(&cfg, &LlmConfig::default(), &ResourcesConfig::default(), cfg.embed);
    assert_eq!(doc_cfg.max_pages, 37);
    assert_eq!(doc_cfg.extraction_timeout_secs, 42);
}

#[test]
fn doc_config_from_propagates_summarization_and_llm() {
    use crate::config::{SummarizationConfig, SummarizationStrategy};
    let cfg = DocumentsConfig {
        summarization: SummarizationConfig {
            enabled: true,
            strategy: SummarizationStrategy::Abstractive,
            max_tokens: Some(150),
        },
        ..Default::default()
    };
    let llm = LlmConfig {
        model: "openai/gpt-4o".to_string(),
        ..Default::default()
    };
    let doc_cfg = doc_config_from(&cfg, &llm, &ResourcesConfig::default(), cfg.embed);
    assert!(doc_cfg.summarization.enabled);
    assert_eq!(doc_cfg.summarization.max_tokens, Some(150));
    assert_eq!(doc_cfg.llm.model, "openai/gpt-4o");
}

#[test]
fn should_extract_document_respects_disabled_flag() {
    let cfg = DocumentsConfig {
        enabled: false,
        ..Default::default()
    };
    let out = should_extract_document(Path::new("dummy.pdf"), &cfg);
    assert!(out.is_none());
}

#[test]
fn should_extract_document_rejects_archives_and_binaries() {
    let cfg = DocumentsConfig::default();
    for path in [
        "vendor/lib.zip",
        "target/app.jar",
        "dist/bundle.tar.gz",
        "build/libfoo.so",
        "pkg/module.wasm",
        "out/Main.class",
        "wheels/pkg-1.0.whl",
        "bin/tool.exe",
        "obj/thing.o",
    ] {
        assert!(
            should_extract_document(Path::new(path), &cfg).is_none(),
            "archive/binary must be denied: {path}"
        );
    }
}

#[test]
fn should_extract_document_allows_real_documents() {
    let cfg = DocumentsConfig::default();
    for path in ["docs/manual.pdf", "notes/readme.txt", "report.csv"] {
        assert!(
            should_extract_document(Path::new(path), &cfg).is_some(),
            "extractable document must pass: {path}"
        );
    }
}

#[test]
fn should_extract_document_honors_extension_denylist_override() {
    let cfg = DocumentsConfig {
        extension_denylist: vec!["pdf".to_string()],
        ..Default::default()
    };
    assert!(should_extract_document(Path::new("docs/manual.pdf"), &cfg).is_none());
    assert!(should_extract_document(Path::new("vendor/lib.zip"), &cfg).is_none());
}

#[test]
fn images_pass_but_audio_video_denied() {
    let cfg = DocumentsConfig::default();
    assert!(should_extract_document(Path::new("assets/photo.png"), &cfg).is_some());
    assert!(should_extract_document(Path::new("clips/audio.mp3"), &cfg).is_none());
    assert!(should_extract_document(Path::new("clips/movie.mp4"), &cfg).is_none());
}

#[test]
fn doc_scope_keeps_default_for_repo_relative_paths() {
    let cfg = crate::config::ConfigV1::with_defaults();
    let scope = doc_scope_for("docs/manual.pdf", "repo:origin", &cfg);
    assert_eq!(scope, "repo:origin");
    assert!(matches!(scope, std::borrow::Cow::Borrowed(_)));
}

#[test]
fn doc_scope_namespaces_external_files_under_their_extra_root() {
    let ext = tempfile::tempdir().expect("tempdir");
    let ext_canonical = std::fs::canonicalize(ext.path()).unwrap();
    let mut cfg = crate::config::ConfigV1::with_defaults();
    cfg.scan.extra_roots = vec![ext.path().to_path_buf()];

    let file_key = ext_canonical.join("pkg/notes.pdf");
    let scope = doc_scope_for(file_key.to_str().unwrap(), "repo:origin", &cfg);
    assert_eq!(scope, format!("path:{}", ext_canonical.to_str().unwrap()));
}

#[test]
fn doc_scope_prefers_the_longest_root_and_respects_segment_boundaries() {
    let base = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(base.path()).unwrap();
    for dir in ["foo", "foo/inner", "foobar"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    let mut cfg = crate::config::ConfigV1::with_defaults();
    cfg.scan.extra_roots = vec![base.join("foo"), base.join("foo/inner"), base.join("foobar")];
    let scope_of = |rel: &str| doc_scope_for(base.join(rel).to_str().unwrap(), "repo:origin", &cfg).into_owned();

    assert_eq!(scope_of("foo/a.pdf"), format!("path:{}", base.join("foo").display()));
    assert_eq!(
        scope_of("foo/inner/a.pdf"),
        format!("path:{}", base.join("foo/inner").display()),
        "nested root wins over its parent"
    );
    assert_eq!(
        scope_of("foobar/a.pdf"),
        format!("path:{}", base.join("foobar").display()),
        "/foo must not claim /foobar"
    );
}

fn seeded_doc_lance(store: &mut Store, paths: &[&str]) -> crate::lance::LanceStore {
    let lance = store.lance_or_open(768, "balanced").expect("open lance").clone();
    for path in paths {
        let row = DocumentRow {
            scope: "repo:s".to_string(),
            path: (*path).to_string(),
            chunk_idx: 0,
            mime_type: "text/plain".to_string(),
            text: "body".to_string(),
            byte_start: 0,
            byte_end: 4,
            embedding: vec![0.25; 768],
        };
        lance.replace_document("repo:s", path, vec![row]).expect("seed row");
        store.upsert_doc(
            *path,
            crate::store::DocEntry {
                config_digest: String::new(),
                hash_hex: "h".to_string(),
                embedding_preset: "balanced".to_string(),
                size_bytes: 1,
                mtime: 0,
                embedded: true,
                embed_attempted: true,
            },
        );
    }
    lance
}

fn doc_rows(lance: &crate::lance::LanceStore) -> Vec<String> {
    let mut paths: Vec<String> = lance
        .search_documents("repo:s", vec![0.25; 768], 50, None)
        .expect("search")
        .into_iter()
        .map(|hit| hit.path)
        .collect();
    paths.sort();
    paths
}

/// The embed-policy purge is what makes `embed_exclude` / `embed_include` / `embed = false`
/// retroactive: rows of newly ineligible documents disappear, eligible ones stay. Exercised against
/// a real LanceDB table (no embedding model involved).
#[test]
fn embed_policy_purge_deletes_vectors_of_newly_ineligible_documents_only() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
    let lance = seeded_doc_lance(&mut store, &["docs/a.pdf", "docs/b.pdf", "notes/c.pdf"]);
    assert_eq!(doc_rows(&lance).len(), 3);

    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.embed = true;
    config.documents.embed_exclude = vec!["docs/b.pdf".to_string()];
    let filters = Filters::build(&config, Vec::new()).unwrap();
    purge_unembedded_documents(&mut store, &config, &filters, "repo:s");
    assert_eq!(
        doc_rows(&lance),
        vec!["docs/a.pdf".to_string(), "notes/c.pdf".to_string()]
    );
    assert!(
        store.lookup_doc("docs/b.pdf").is_some(),
        "the document stays indexed, only its vectors go"
    );

    config.documents.embed_exclude = Vec::new();
    config.documents.embed_include = vec!["notes/**".to_string()];
    let filters = Filters::build(&config, Vec::new()).unwrap();
    purge_unembedded_documents(&mut store, &config, &filters, "repo:s");
    assert_eq!(
        doc_rows(&lance),
        vec!["notes/c.pdf".to_string()],
        "embed_include narrows retroactively"
    );

    config.documents.embed = false;
    let filters = Filters::build(&config, Vec::new()).unwrap();
    purge_unembedded_documents(&mut store, &config, &filters, "repo:s");
    assert!(doc_rows(&lance).is_empty(), "embed = false removes every vector row");
}

#[test]
fn embed_policy_purge_never_creates_the_vector_store() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
    store.upsert_doc(
        "docs/a.pdf",
        crate::store::DocEntry {
            config_digest: String::new(),
            hash_hex: "h".to_string(),
            embedding_preset: "balanced".to_string(),
            size_bytes: 1,
            mtime: 0,
            embedded: true,
            embed_attempted: true,
        },
    );
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.embed = false;
    let filters = Filters::build(&config, Vec::new()).unwrap();
    purge_unembedded_documents(&mut store, &config, &filters, "repo:s");
    assert!(store.lance.is_none() && !store.lance_dir_exists());
}
