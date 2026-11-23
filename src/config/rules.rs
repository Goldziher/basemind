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

use super::{CodeSearchConfig, ConfigV1, DocumentsConfig, ResourcesConfig};

/// Length of a config digest in hex characters.
pub const DIGEST_LEN: usize = 16;

const GLOB_META: &[char] = &['*', '?', '[', ']', '{', '}', '\\'];

/// Expand a user pattern into the globs that implement its documented meaning.
///
/// A pattern with glob metacharacters is used as written, minus a leading `./` or `/` and a trailing
/// `/` (which makes it also match everything beneath). A bare pattern (no metacharacters) is
/// gitignore-like: `generated` matches a path segment of that name at any depth and everything
/// beneath it; `docs/api` (contains a slash) is anchored to the root and matches that path and
/// everything beneath it.
pub fn expand_pattern(pattern: &str) -> Vec<String> {
    // A leading `./` or `/` and a trailing `/` are path decoration in every form, glob or not.
    let trimmed = pattern.trim_start_matches("./").trim_start_matches('/');
    let dir_only = trimmed.ends_with('/');
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() {
        return vec![pattern.to_string()];
    }
    if trimmed.contains(GLOB_META) {
        // `build/*/` names directories: match them and everything beneath.
        return if dir_only {
            vec![trimmed.to_string(), format!("{trimmed}/**")]
        } else {
            vec![trimmed.to_string()]
        };
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
        if pattern.trim_start().starts_with('!') {
            return Err(format!("{pattern:?}: negated patterns are not supported"));
        }
        for expanded in expand_pattern(pattern) {
            let glob = Glob::new(&expanded).map_err(|e| format!("{pattern:?}: {e}"))?;
            builder.add(glob);
        }
    }
    builder.build().map_err(|e| e.to_string())
}

/// An ordered glob list with gitignore semantics: the last pattern that matches a path decides, and
/// a `!pattern` entry re-includes what an earlier pattern matched. Built by [`compile_rules`].
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    /// `(patterns, negated)` in list order.
    rules: Vec<(GlobSet, bool)>,
}

impl RuleSet {
    /// Does the list select `key`? True when the last matching entry is a plain pattern, false when
    /// it is a `!` entry or nothing matches.
    pub fn is_match(&self, key: &str) -> bool {
        self.rules
            .iter()
            .rev()
            .find(|(set, _)| set.is_match(key))
            .is_some_and(|(_, negated)| !negated)
    }
}

/// Compile `patterns` into a [`RuleSet`]. Unlike [`compile_patterns`] a leading `!` is accepted and
/// marks a re-include; the rest of the entry expands exactly as [`expand_pattern`] describes.
pub fn compile_rules<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<RuleSet, String> {
    let rules = patterns
        .into_iter()
        .map(|pattern| {
            let trimmed = pattern.trim_start();
            let (body, negated) = trimmed.strip_prefix('!').map_or((pattern, false), |rest| (rest, true));
            compile_patterns([body])
                .map(|set| (set, negated))
                .map_err(|e| format!("{pattern:?}: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RuleSet { rules })
}

/// Classifies repo-relative paths as test files from `[scan] test_globs`, the one list behind the
/// code map's `is_test` flag and the document tier's default test exclusion.
#[derive(Debug, Clone)]
pub struct TestPaths(GlobSet);

impl TestPaths {
    /// Compile `globs`; an invalid glob is a config error.
    pub fn new(globs: &[String]) -> Result<Self, String> {
        compile_patterns(globs.iter().map(String::as_str)).map(Self)
    }

    /// The matcher for `[scan] test_globs`, falling back to the default list when the configured
    /// one does not compile (validation rejects that earlier, so this is a safety net).
    pub fn from_config(cfg: &ConfigV1) -> Self {
        Self::new(&cfg.scan.test_globs).unwrap_or_else(|_| {
            let defaults = super::documents::TEST_PATH_GLOBS
                .iter()
                .map(|g| (*g).to_string())
                .collect::<Vec<_>>();
            Self(compile_patterns(defaults.iter().map(String::as_str)).unwrap_or_default())
        })
    }

    /// Is the repo-relative, forward-slash `path` a test path?
    pub fn is_test(&self, path: &str) -> bool {
        self.0.is_match(path)
    }
}

/// Post-deserialization checks the JSON schema cannot express: non-empty `[scan] include`, valid
/// globs in every list, no empty `[analysis] entry_points` pattern, and a well-formed `[languages]`
/// table.
pub fn validate(cfg: &ConfigV1) -> Result<(), String> {
    if cfg.scan.include.is_empty() {
        return Err(
            "[scan] include is empty, which would index nothing; remove the key to use the default \
                    [\"**/*\"] or list the globs to keep"
                .to_string(),
        );
    }
    let lists: [(&str, &[String]); 4] = [
        ("[scan] include", &cfg.scan.include),
        ("[scan] test_globs", &cfg.scan.test_globs),
        ("[scan] exclude", &cfg.scan.exclude),
        ("[analysis] entry_points", &cfg.analysis.entry_points),
    ];
    let ordered: [(&str, &[String]); 7] = [
        ("[scan] list_only", &cfg.scan.list_only),
        ("[documents] include", &cfg.documents.include),
        ("[documents] exclude", &cfg.documents.exclude),
        ("[documents] embed_include", &cfg.documents.embed_include),
        ("[documents] embed_exclude", &cfg.documents.embed_exclude),
        ("[code_search] embed_include", &cfg.code_search.embed_include),
        ("[code_search] embed_exclude", &cfg.code_search.embed_exclude),
    ];
    for (name, list) in ordered {
        compile_rules(list.iter().map(String::as_str)).map_err(|e| format!("invalid glob in {name}: {e}"))?;
    }
    if cfg
        .analysis
        .entry_points
        .iter()
        .any(|pattern| pattern.trim().is_empty())
    {
        return Err("[analysis] entry_points holds an empty pattern; remove it or name the entry files".to_string());
    }
    for (name, list) in lists {
        compile_patterns(list.iter().map(String::as_str)).map_err(|e| {
            let negated = list.iter().any(|pattern| pattern.trim_start().starts_with('!'));
            let advice = match name {
                _ if !negated => "",
                "[analysis] entry_points" => "; list only the files to treat as entry points",
                _ => "; list what to keep in `include` instead",
            };
            format!("invalid glob in {name}: {e}{advice}")
        })?;
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
pub fn doc_digest(cfg: &DocumentsConfig, resources: &ResourcesConfig) -> String {
    // The splitter cutovers decide which chunker a text file gets, so they belong in the digest, but only
    // when moved off their defaults: folding the defaults in would change every existing digest and
    // re-extract every document on upgrade.
    let defaults = DocumentsConfig::default();
    let cutovers = (cfg.markdown_chunk_max_bytes, cfg.plain_text_chunk_max_bytes);
    // `doc/2`: Markdown output, repeated table headers, chunk metadata and snapped chunk starts
    // (ADR-0029 rule 10) change every document's chunks. `doc/3`: text and data files chunked from
    // the file, so every chunk of them cites its lines. `doc/4`: Markdown lines found across
    // paragraphs xberg reflowed. `doc/5`: prose chunked from the file overlaps by whole words where
    // its overlap holds no line start.
    let mut tag = if cutovers == (defaults.markdown_chunk_max_bytes, defaults.plain_text_chunk_max_bytes) {
        "doc/5".to_string()
    } else {
        format!("doc/5/cutover={},{}", cutovers.0, cutovers.1)
    };
    // `extract::doc_tiers` is compiled only with `documents`, and this function is not.
    tag.push_str(&missing_tier_suffix(
        cfg!(feature = "documents"),
        cfg!(feature = "documents-ocr"),
        cfg!(feature = "documents-ner"),
        cfg!(feature = "documents-heic"),
    ));
    digest_of(
        &tag,
        &(
            cfg.max_characters,
            cfg.overlap,
            cfg.max_chunks_per_document,
            cfg.max_pages,
            // The slot `extract_archives` held before archive extraction was removed. Its default was
            // `false`, so a config that left it there keeps its digest; one that set `true`
            // re-extracts its documents once.
            false,
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
            // The slot `[llm] model` held before the LLM backends were removed. Its default was
            // empty, so a config that left it unset keeps its digest; one that set a model
            // re-extracts its documents once.
            "",
        ),
    )
}

/// Digest-tag suffix naming the document tiers this binary lacks. A document extracted without OCR,
/// NER or HEIC decoding is missing text or entities that a build with the tier produces, so the two
/// builds must not share a digest: swapping binaries then re-extracts. Empty when every tier is
/// compiled in, which keeps the digest of a full build what it was before the tiers existed. HEIC
/// decoding needs OCR, so a build without OCR is tagged `/no-ocr` alone for it.
fn missing_tier_suffix(documents: bool, ocr: bool, ner: bool, heic: bool) -> String {
    if !documents {
        return String::new();
    }
    [(ocr, "/no-ocr"), (ner, "/no-ner"), (heic || !ocr, "/no-heic")]
        .into_iter()
        .filter_map(|(present, tag)| (!present).then_some(tag))
        .collect()
}

/// Fingerprint of the limits a document's extraction ran under, for its timeout tombstone (see
/// [`crate::store_doc_tombstone`]). A tombstone is honoured only while this is unchanged, so raising
/// `extraction_timeout_secs`, moving a chunker cutover or `max_pages`, or changing any setting that
/// shapes extraction (`doc_digest`, passed in as already computed) retries the document.
pub fn doc_guard_digest(cfg: &DocumentsConfig, doc_digest: &str) -> String {
    digest_of(
        "doc-guard/1",
        &(
            doc_digest,
            cfg.extraction_timeout_secs,
            cfg.markdown_chunk_max_bytes,
            cfg.plain_text_chunk_max_bytes,
            cfg.max_pages,
        ),
    )
}

/// Fingerprint of every setting that decides what a scan reads, extracts or stores: the walk
/// (`[scan]`, `[languages]`), the extractors (`[code_intel]`, `[code_search]`, `[documents]`) and the
/// resource limits they run under. A config edit that leaves it unchanged (a new file holding the
/// defaults, a `[mcp]` or `[watch]` tweak) cannot change what an index holds, so it owes no rescan.
pub fn scan_inputs_digest(cfg: &ConfigV1) -> String {
    digest_of(
        "scan-inputs/1",
        &(
            &cfg.scan,
            &cfg.languages,
            &cfg.code_intel,
            &cfg.code_search,
            &cfg.documents,
            &cfg.resources,
        ),
    )
}

/// Fingerprint of every setting a daemon-hosted read stack is built from: the scan inputs
/// ([`scan_inputs_digest`]) and `watch.debounce_ms`, which its watcher reads once at start. A stack
/// built under a different digest is rebuilt; an edit that leaves it unchanged keeps the stack.
pub fn read_stack_digest(cfg: &ConfigV1) -> String {
    digest_of("read-stack/1", &(scan_inputs_digest(cfg), cfg.watch.debounce_ms))
}

/// Fingerprint of everything that decides which derived rows (vectors, keyword postings) should
/// exist for a path. When it differs from the value stored in the index, the next full scan
/// re-flushes eligible files and purges the rows of files that are no longer eligible.
pub fn embed_policy_digest(cfg: &ConfigV1) -> String {
    digest_of(
        // Rebuild keyword postings that older scans may have deleted for duplicate content.
        "embed-policy/2",
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

    fn ruled(patterns: &[&str], path: &str) -> bool {
        compile_rules(patterns.iter().copied())
            .expect("compiles")
            .is_match(path)
    }

    #[test]
    fn rule_sets_let_the_last_matching_entry_decide() {
        for (patterns, path, expected) in [
            (&["**/*.html"][..], "a/b.html", true),
            (&["**/*.html", "!docs/**"][..], "docs/b.html", false),
            (&["**/*.html", "!docs/**"][..], "web/b.html", true),
            (&["!docs/**", "**/*.html"][..], "docs/b.html", true),
            (
                &["**/*.html", "!docs/**", "docs/drafts"][..],
                "docs/drafts/b.html",
                true,
            ),
            (&["!docs/**"][..], "docs/b.html", false),
            (&[][..], "docs/b.html", false),
        ] {
            assert_eq!(ruled(patterns, path), expected, "{patterns:?} on {path}");
        }
    }

    #[test]
    fn rule_sets_reject_an_invalid_glob_behind_a_negation() {
        let err = compile_rules(["!src/[unclosed"]).expect_err("unbalanced class");
        assert!(err.contains("src/[unclosed"), "{err}");
    }

    #[test]
    fn document_lists_validate_with_negations_but_scan_lists_do_not() {
        let mut cfg = ConfigV1::with_defaults();
        cfg.documents.exclude.push("!docs/**".to_string());
        validate(&cfg).expect("a `!` entry is valid in a document list");
        cfg.scan.exclude.push("!docs/**".to_string());
        let err = validate(&cfg).expect_err("scan lists stay negation-free");
        assert!(err.contains("[scan] exclude"), "{err}");
    }

    #[test]
    fn test_paths_follow_the_configured_globs() {
        let default = TestPaths::from_config(&ConfigV1::with_defaults());
        for path in [
            "tests/a.rs",
            "pkg/test/a.py",
            "web/__tests__/a.ts",
            "src/foo_test.go",
            "pkg/test_foo.py",
            "web/a.spec.ts",
            "web/a.test.js",
            "pkg/conftest.py",
            "e2e/flow.ts",
            "app/mocks/api.ts",
            "app/fixtures/a.json",
            "ui/__snapshots__/a.snap",
        ] {
            assert!(default.is_test(path), "{path} is a test path");
        }
        for path in ["src/lib.rs", "docs/testing.md", "src/contest.py", "latest/a.py"] {
            assert!(!default.is_test(path), "{path} is not a test path");
        }
        let custom = TestPaths::new(&["qa/**".to_string()]).expect("compiles");
        assert!(custom.is_test("qa/a.rs") && !custom.is_test("tests/a.rs"));
        assert!(TestPaths::new(&["a/[".to_string()]).is_err());
    }

    #[test]
    fn extended_test_globs_match_their_shape_and_not_look_alikes() {
        let default = TestPaths::from_config(&ConfigV1::with_defaults());
        for (positive, negative) in [
            ("ci/run.bats", "ci/run.bat"),
            ("ui/a.snap", "ui/a.snapshot"),
            ("ui/a.stories.tsx", "ui/a.story.tsx"),
            ("ui/a.mock.ts", "ui/a.mocking.ts"),
            ("ui/a.mocks.ts", "ui/a.mockery.ts"),
            ("ui/__mocks__/a.ts", "ui/__mock__/a.ts"),
            ("ui/mock/a.ts", "ui/mockery/a.ts"),
            ("pkg/testing/a.py", "pkg/contesting/a.py"),
            ("web/playwright/a.ts", "web/playwrighty/a.ts"),
            ("web/cypress/a.ts", "web/cypresses/a.ts"),
            ("ui/stories/a.tsx", "ui/mystories/a.tsx"),
            ("src/FooTest.java", "src/Contest.java"),
            ("src/FooTests.java", "src/Attests.java"),
            ("src/FooTest.kt", "src/Contest.kt"),
            ("lib/foo_spec.rb", "lib/foo.rb"),
            ("src/foo_tests.rs", "src/attestation.rs"),
            ("pkg/test_utils/a.py", "pkg/mytest_utils/a.py"),
            ("pkg/testutils/a.go", "pkg/contestutils/a.go"),
            ("pkg/tests.py", "pkg/attests.py"),
            ("web/a.e2e.ts", "web/e2e.ts"),
            ("web/a.cy.ts", "web/a.cyan.ts"),
            ("src/foo_test.py", "src/latest/x.py"),
        ] {
            assert!(default.is_test(positive), "{positive} is a test path");
            assert!(!default.is_test(negative), "{negative} is not a test path");
        }
        for path in ["src/contest.py", "attestation.go", "src/latest/x.py"] {
            assert!(!default.is_test(path), "{path} is not a test path");
        }
    }

    #[test]
    fn scan_inputs_digest_tracks_scan_settings_and_ignores_the_rest() {
        let base = ConfigV1::with_defaults();
        let mut watch_only = base.clone();
        watch_only.watch.debounce_ms += 1;
        assert_eq!(scan_inputs_digest(&base), scan_inputs_digest(&watch_only));

        let mut excluded = base.clone();
        excluded.scan.exclude.push("generated".to_string());
        assert_ne!(scan_inputs_digest(&base), scan_inputs_digest(&excluded));
    }

    #[test]
    fn read_stack_digest_adds_the_watcher_debounce_to_the_scan_inputs() {
        let base = ConfigV1::with_defaults();
        let mut debounced = base.clone();
        debounced.watch.debounce_ms += 1;
        assert_ne!(read_stack_digest(&base), read_stack_digest(&debounced));

        let mut excluded = base.clone();
        excluded.scan.exclude.push("generated".to_string());
        assert_ne!(read_stack_digest(&base), read_stack_digest(&excluded));

        assert_eq!(read_stack_digest(&base), read_stack_digest(&base.clone()));
    }

    #[test]
    fn doc_digest_ignores_reserved_fields_but_tracks_live_ones() {
        let resources = ResourcesConfig::default();
        let base = DocumentsConfig::default();
        let digest = |cfg: &DocumentsConfig| doc_digest(cfg, &resources);

        let mut reserved = base.clone();
        reserved.language.preferred_languages = vec!["fra".into()];
        reserved.ocr.languages = vec!["deu".into()];
        reserved.ocr.backend = crate::config::documents::OcrBackend::Paddle;
        assert_eq!(
            digest(&base),
            digest(&reserved),
            "reserved keys must not invalidate documents"
        );

        let mut archives = base.clone();
        archives.extract_archives = !archives.extract_archives;
        assert_eq!(
            digest(&base),
            digest(&archives),
            "extract_archives is ignored, so toggling it must not invalidate documents"
        );

        let mut live = base.clone();
        live.language.detect_multiple = !live.language.detect_multiple;
        assert_ne!(digest(&base), digest(&live));
        let mut live = base.clone();
        live.max_pages += 1;
        assert_ne!(digest(&base), digest(&live));
    }

    #[test]
    fn the_digest_tag_names_only_the_tiers_a_documents_build_lacks() {
        let cases = [
            ((true, true, true), ""),
            ((true, true, false), "/no-heic"),
            ((false, true, false), "/no-ocr"),
            ((true, false, true), "/no-ner"),
            ((true, false, false), "/no-ner/no-heic"),
            ((false, false, false), "/no-ocr/no-ner"),
        ];
        for ((ocr, ner, heic), suffix) in cases {
            assert_eq!(
                missing_tier_suffix(true, ocr, ner, heic),
                suffix,
                "ocr={ocr} ner={ner} heic={heic}"
            );
            assert_eq!(
                missing_tier_suffix(false, ocr, ner, heic),
                "",
                "no documents feature, no suffix"
            );
        }
    }

    #[test]
    fn doc_digest_tracks_chunker_cutovers_only_off_their_defaults() {
        let resources = ResourcesConfig::default();
        let base = DocumentsConfig::default();
        let digest = |cfg: &DocumentsConfig| doc_digest(cfg, &resources);

        let mut moved = base.clone();
        moved.markdown_chunk_max_bytes += 1;
        assert_ne!(digest(&base), digest(&moved), "a moved cutover re-chunks documents");
        let mut moved_plain = base.clone();
        moved_plain.plain_text_chunk_max_bytes += 1;
        assert_ne!(digest(&base), digest(&moved_plain));
        assert_ne!(digest(&moved), digest(&moved_plain));
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
    fn path_decoration_is_normalised_for_glob_patterns_too() {
        // Each form a user might copy from a .gitignore must select `src/a.rs`.
        for pattern in [
            "./src/**",
            "/src/**",
            "src/**",
            "/src/*.rs",
            "./src/*.rs",
            "src/*/",
            "/src/*/",
        ] {
            let hit = if pattern.ends_with('/') {
                "src/sub/a.rs"
            } else {
                "src/a.rs"
            };
            assert!(matches(&[pattern], hit), "{pattern:?} should match {hit}");
        }
        assert_eq!(expand_pattern("/src/**"), expand_pattern("src/**"));
        assert!(
            matches(&["build/*/"], "build/out/a.rs"),
            "trailing slash also covers everything beneath"
        );
    }

    #[test]
    fn negated_patterns_are_rejected_not_taken_literally() {
        let err = compile_patterns(["!foo"]).expect_err("negation");
        assert!(err.contains("negated"), "{err}");
    }

    #[test]
    fn an_analysis_list_names_its_own_advice_and_refuses_an_empty_pattern() {
        let mut cfg = ConfigV1::with_defaults();
        cfg.analysis.entry_points = vec!["!tools/*.py".to_string()];
        let err = validate(&cfg).expect_err("negation");
        assert!(
            err.contains("[analysis] entry_points") && err.contains("entry points") && !err.contains("`include`"),
            "{err}"
        );
        cfg.scan.exclude = vec!["!vendor".to_string()];
        cfg.analysis.entry_points = Vec::new();
        let err = validate(&cfg).expect_err("negation");
        assert!(err.contains("`include`"), "{err}");
        cfg.scan.exclude = Vec::new();
        cfg.analysis.entry_points = vec![String::new()];
        let err = validate(&cfg).expect_err("empty");
        assert!(err.contains("empty pattern"), "{err}");
        cfg.analysis.entry_points = vec!["tools/*.py".to_string()];
        validate(&cfg).expect("a valid list");
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
    fn doc_guard_digest_tracks_the_limits_a_timeout_depends_on() {
        let base = DocumentsConfig::default();
        let digest = |cfg: &DocumentsConfig| doc_guard_digest(cfg, "d");
        let same = digest(&base);
        assert_eq!(digest(&base.clone()), same);
        for changed in [
            DocumentsConfig {
                extraction_timeout_secs: base.extraction_timeout_secs + 1,
                ..base.clone()
            },
            DocumentsConfig {
                markdown_chunk_max_bytes: base.markdown_chunk_max_bytes + 1,
                ..base.clone()
            },
            DocumentsConfig {
                plain_text_chunk_max_bytes: base.plain_text_chunk_max_bytes + 1,
                ..base.clone()
            },
            DocumentsConfig {
                max_pages: base.max_pages + 1,
                ..base.clone()
            },
        ] {
            assert_ne!(digest(&changed), same);
        }
        assert_ne!(doc_guard_digest(&base, "other-doc-digest"), same);
        // Scan-level budgets do not decide whether one document times out.
        let budgets = DocumentsConfig {
            phase_budget_secs: base.phase_budget_secs + 1,
            max_abandoned_extractions: base.max_abandoned_extractions + 1,
            ..base.clone()
        };
        assert_eq!(digest(&budgets), same);
    }

    #[test]
    fn doc_digest_tracks_extraction_knobs_only() {
        let (cfg, res) = (DocumentsConfig::default(), ResourcesConfig::default());
        let digest = doc_digest(&cfg, &res);
        let mut other = cfg.clone();
        other.max_characters += 1;
        assert_ne!(doc_digest(&other, &res), digest);
        let mut other = cfg.clone();
        other.keywords.enabled = !other.keywords.enabled;
        assert_ne!(doc_digest(&other, &res), digest);
        let mut other = cfg.clone();
        other.summarization.enabled = !other.summarization.enabled;
        assert_ne!(doc_digest(&other, &res), digest);
        let mut other = cfg.clone();
        other.embedding_preset = "quality".to_string();
        other.embed_exclude = vec!["x".to_string()];
        other.embed_max_threads = 3;
        assert_eq!(
            doc_digest(&other, &res),
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
        other.documents.embed_exclude = Vec::new();
        assert_ne!(
            embed_policy_digest(&other),
            digest,
            "an index built before structured data left the embed scope reconciles on its next scan"
        );
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
