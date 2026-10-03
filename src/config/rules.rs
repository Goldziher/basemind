//! Glob rules and config fingerprints shared by config validation and the scanner.
//!
//! Every user-supplied glob list (`[scan] include/exclude`, `[documents] include/exclude/
//! embed_include/embed_exclude`, `[code_search] embed_include/embed_exclude`) is compiled through
//! [`compile_patterns`], so bare names behave the same everywhere and an invalid glob is a config
//! error rather than a silently dropped rule.
//!
//! The digests at the bottom are the staleness fingerprints: a cached chunk / document is reusable
//! only when it was produced under the same digest, so changing a chunk-size knob re-processes the
//! affected files instead of serving stale data.

use globset::{Glob, GlobSet, GlobSetBuilder};

use super::{CodeSearchConfig, ConfigV1, DocumentsConfig, LlmConfig, ResourcesConfig};

/// Length of a config digest in hex characters.
pub const DIGEST_LEN: usize = 16;

const GLOB_META: &[char] = &['*', '?', '[', ']', '{', '}', '\\'];

/// Expand a user pattern into the globs that implement its documented meaning.
///
/// A pattern with glob metacharacters is used verbatim. A bare pattern (no metacharacters) is
/// gitignore-like: `generated` matches a path segment of that name at any depth and everything
/// beneath it; `docs/api` (contains a slash) is anchored to the root and matches that path and
/// everything beneath it.
pub fn expand_pattern(pattern: &str) -> Vec<String> {
    if pattern.contains(GLOB_META) {
        return vec![pattern.to_string()];
    }
    let trimmed = pattern
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/');
    if trimmed.is_empty() {
        return vec![pattern.to_string()];
    }
    if trimmed.contains('/') {
        vec![trimmed.to_string(), format!("{trimmed}/**")]
    } else {
        vec![format!("**/{trimmed}"), format!("**/{trimmed}/**")]
    }
}

/// Compile `patterns` into one [`GlobSet`], expanding bare names via [`expand_pattern`].
pub fn compile_patterns<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        for expanded in expand_pattern(pattern) {
            let glob = Glob::new(&expanded).map_err(|e| format!("{pattern:?}: {e}"))?;
            builder.add(glob);
        }
    }
    builder.build().map_err(|e| e.to_string())
}

/// Post-deserialization checks the JSON schema cannot express: non-empty `[scan] include`, valid
/// globs in every list, and a well-formed `[languages]` table.
pub fn validate(cfg: &ConfigV1) -> Result<(), String> {
    if cfg.scan.include.is_empty() {
        return Err(
            "[scan] include is empty, which would index nothing; remove the key to use the default \
                    [\"**/*\"] or list the globs to keep"
                .to_string(),
        );
    }
    let lists: [(&str, &[String]); 8] = [
        ("[scan] include", &cfg.scan.include),
        ("[scan] exclude", &cfg.scan.exclude),
        ("[documents] include", &cfg.documents.include),
        ("[documents] exclude", &cfg.documents.exclude),
        ("[documents] embed_include", &cfg.documents.embed_include),
        ("[documents] embed_exclude", &cfg.documents.embed_exclude),
        ("[code_search] embed_include", &cfg.code_search.embed_include),
        ("[code_search] embed_exclude", &cfg.code_search.embed_exclude),
    ];
    for (name, list) in lists {
        compile_patterns(list.iter().map(String::as_str)).map_err(|e| format!("invalid glob in {name}: {e}"))?;
    }
    crate::lang_rules::LangRules::from_config(&cfg.languages)?;
    Ok(())
}

/// Lowercase, dot-less form of a file extension (`".PDF"` -> `"pdf"`).
pub fn normalize_extension(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_ascii_lowercase()
}

fn digest_of<T: serde::Serialize>(tag: &str, value: &T) -> String {
    let mut bytes = tag.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend(serde_json::to_vec(value).expect("config digest inputs always serialize"));
    let hash = crate::hashing::hex(&crate::hashing::hash_bytes(&bytes));
    hash[..DIGEST_LEN].to_string()
}

/// Fingerprint of the `[code_search]` settings that shape a file's cached chunks and keyword
/// postings. Embedding preset / on-off are tracked separately (they are checked against the
/// sidecar's own `embedding_model` and vectors).
pub fn code_digest(cfg: &CodeSearchConfig) -> String {
    digest_of("code/1", &(cfg.max_characters, cfg.overlap, cfg.max_chunks_per_file))
}

/// Fingerprint of the settings that shape a document's extracted content, chunks, keywords,
/// entities and summary. `output` (MCP response format) and the embedding preset / threads are
/// deliberately excluded: they do not change what is stored.
pub fn doc_digest(cfg: &DocumentsConfig, resources: &ResourcesConfig, llm: &LlmConfig) -> String {
    digest_of(
        "doc/1",
        &(
            cfg.max_characters,
            cfg.overlap,
            cfg.max_chunks_per_document,
            cfg.max_pages,
            cfg.extract_archives,
            // Only the knobs that reach the extractor: `preferred_languages` and `[ocr]` are
            // reserved, so editing them must not re-extract every document.
            (
                cfg.language.auto_detect,
                cfg.language.min_confidence.to_bits(),
                cfg.language.detect_multiple,
            ),
            &cfg.keywords,
            &cfg.ner,
            &cfg.summarization,
            resources.document_models,
            &llm.model,
        ),
    )
}

/// Fingerprint of everything that decides which derived rows (vectors, keyword postings) should
/// exist for a path. When it differs from the value stored in the index, the next full scan
/// re-flushes eligible files and purges the rows of files that are no longer eligible.
pub fn embed_policy_digest(cfg: &ConfigV1) -> String {
    digest_of(
        "embed-policy/1",
        &(
            cfg.code_search.enabled,
            cfg.code_search.embed,
            &cfg.code_search.embed_include,
            &cfg.code_search.embed_exclude,
            cfg.documents.enabled,
            cfg.documents.embed,
            &cfg.documents.embed_include,
            &cfg.documents.embed_exclude,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(patterns: &[&str], path: &str) -> bool {
        compile_patterns(patterns.iter().copied())
            .expect("compiles")
            .is_match(path)
    }

    #[test]
    fn doc_digest_ignores_reserved_fields_but_tracks_live_ones() {
        let resources = ResourcesConfig::default();
        let llm = LlmConfig::default();
        let base = DocumentsConfig::default();
        let digest = |cfg: &DocumentsConfig| doc_digest(cfg, &resources, &llm);

        let mut reserved = base.clone();
        reserved.language.preferred_languages = vec!["fra".into()];
        reserved.ocr.languages = vec!["deu".into()];
        reserved.ocr.backend = crate::config::documents::OcrBackend::Paddle;
        assert_eq!(
            digest(&base),
            digest(&reserved),
            "reserved keys must not invalidate documents"
        );

        let mut live = base.clone();
        live.language.detect_multiple = !live.language.detect_multiple;
        assert_ne!(digest(&base), digest(&live));
        let mut live = base.clone();
        live.max_pages += 1;
        assert_ne!(digest(&base), digest(&live));
    }

    #[test]
    fn bare_name_matches_segment_at_any_depth_and_everything_beneath() {
        assert!(matches(&["generated"], "generated"));
        assert!(matches(&["generated"], "generated/a.rs"));
        assert!(matches(&["generated"], "src/generated/deep/a.rs"));
        assert!(!matches(&["generated"], "src/generated_code/a.rs"));
        assert!(!matches(&["generated"], "src/regenerated/a.rs"));
    }

    #[test]
    fn bare_path_with_slash_is_anchored_to_the_root() {
        assert!(matches(&["docs/api"], "docs/api/x.md"));
        assert!(matches(&["./docs/api/"], "docs/api/x.md"));
        assert!(!matches(&["docs/api"], "other/docs/api/x.md"));
    }

    #[test]
    fn patterns_with_metacharacters_are_used_verbatim() {
        assert!(matches(&["**/*.rs"], "a/b/c.rs"));
        assert!(!matches(&["*.rs"], "README.md"));
        assert!(matches(&["src/**"], "src/a/b.rs"));
        assert!(!matches(&["src/*.rs"], "lib/a.rs"));
        assert_eq!(expand_pattern("**/gen*"), vec!["**/gen*".to_string()]);
    }

    #[test]
    fn glob_semantics_are_case_sensitive_and_star_crosses_slashes() {
        assert!(matches(&["src/*.rs"], "src/a/b.rs"), "`*` crosses `/`");
        assert!(!matches(&["**/*.RS"], "a/b.rs"), "matching is case-sensitive");
    }

    #[test]
    fn invalid_glob_is_an_error() {
        let err = compile_patterns(["src/[unclosed"]).expect_err("unbalanced class");
        assert!(err.contains("src/[unclosed"), "{err}");
    }

    #[test]
    fn validate_rejects_empty_include_and_bad_globs() {
        let mut cfg = ConfigV1::with_defaults();
        validate(&cfg).expect("defaults are valid");
        cfg.scan.include = Vec::new();
        assert!(
            validate(&cfg)
                .expect_err("empty include")
                .contains("[scan] include is empty")
        );

        let mut cfg = ConfigV1::with_defaults();
        cfg.documents.embed_include = vec!["a/[".to_string()];
        assert!(
            validate(&cfg)
                .expect_err("bad glob")
                .contains("[documents] embed_include")
        );

        let mut cfg = ConfigV1::with_defaults();
        cfg.code_search.embed_exclude = vec!["a/[".to_string()];
        assert!(
            validate(&cfg)
                .expect_err("bad glob")
                .contains("[code_search] embed_exclude")
        );
    }

    #[test]
    fn extension_normalization_strips_dot_and_case() {
        assert_eq!(normalize_extension(".PDF"), "pdf");
        assert_eq!(normalize_extension(" docx "), "docx");
    }

    #[test]
    fn code_digest_tracks_chunk_knobs_only() {
        let base = CodeSearchConfig::default();
        let digest = code_digest(&base);
        assert_eq!(digest.len(), DIGEST_LEN);
        let mut other = base.clone();
        other.max_characters += 1;
        assert_ne!(code_digest(&other), digest);
        let mut other = base.clone();
        other.overlap += 1;
        assert_ne!(code_digest(&other), digest);
        let mut other = base.clone();
        other.max_chunks_per_file += 1;
        assert_ne!(code_digest(&other), digest);
        let mut other = base;
        other.embed = true;
        other.embed_exclude = vec!["x".to_string()];
        assert_eq!(
            code_digest(&other),
            digest,
            "embed knobs are not part of the chunk digest"
        );
    }

    #[test]
    fn doc_digest_tracks_extraction_knobs_only() {
        let (cfg, res, llm) = (
            DocumentsConfig::default(),
            ResourcesConfig::default(),
            LlmConfig::default(),
        );
        let digest = doc_digest(&cfg, &res, &llm);
        let mut other = cfg.clone();
        other.max_characters += 1;
        assert_ne!(doc_digest(&other, &res, &llm), digest);
        let mut other = cfg.clone();
        other.keywords.enabled = !other.keywords.enabled;
        assert_ne!(doc_digest(&other, &res, &llm), digest);
        let mut other = cfg.clone();
        other.summarization.enabled = !other.summarization.enabled;
        assert_ne!(doc_digest(&other, &res, &llm), digest);
        let mut other = cfg.clone();
        other.embedding_preset = "quality".to_string();
        other.embed_exclude = vec!["x".to_string()];
        other.embed_max_threads = 3;
        assert_eq!(
            doc_digest(&other, &res, &llm),
            digest,
            "embedding knobs are tracked elsewhere"
        );
    }

    #[test]
    fn embed_policy_digest_tracks_eligibility_knobs() {
        let base = ConfigV1::with_defaults();
        let digest = embed_policy_digest(&base);
        let mut other = base.clone();
        other.documents.embed_exclude = vec!["x".to_string()];
        assert_ne!(embed_policy_digest(&other), digest);
        let mut other = base.clone();
        other.code_search.enabled = false;
        assert_ne!(embed_policy_digest(&other), digest);
        let mut other = base;
        other.code_search.max_characters += 1;
        assert_eq!(embed_policy_digest(&other), digest);
    }

    #[test]
    fn a_lone_slash_is_used_verbatim_and_matches_nothing() {
        assert_eq!(expand_pattern("/"), vec!["/".to_string()]);
        assert!(!matches(&["/"], "a"));
        assert!(!matches(&["/"], "src/lib.rs"));
    }

    proptest::proptest! {
        #[test]
        fn compile_patterns_never_panics(pattern in ".{0,64}") {
            let _ = compile_patterns([pattern.as_str()]);
        }

        #[test]
        fn a_meta_free_pattern_always_compiles(pattern in "[a-zA-Z0-9_./ -]{0,32}") {
            proptest::prop_assert!(compile_patterns([pattern.as_str()]).is_ok());
        }

        #[test]
        fn a_bare_name_matches_whole_segments_only(name in "[a-z][a-z0-9_]{0,8}") {
            let set = compile_patterns([name.as_str()]).unwrap();
            let nested = format!("{name}/x/y");
            let deep = format!("0/{name}/b");
            let longer = format!("0/{name}q");
            let prefixed = format!("q{name}/0");
            proptest::prop_assert!(set.is_match(&name));
            proptest::prop_assert!(set.is_match(&nested));
            proptest::prop_assert!(set.is_match(&deep));
            proptest::prop_assert!(!set.is_match(&longer));
            proptest::prop_assert!(!set.is_match(&prefixed));
        }
    }
}
