//! Loads real `basemind.toml` files through the public entry points. The schema is the runtime
//! validator (`additionalProperties: false`), so a struct field added without regenerating it is
//! rejected here; and the trust gate must apply on every loader, not only when called directly.

use std::fs;
use std::sync::{Mutex, MutexGuard, PoisonError};

use basemind::config::{self, ApiKey, DocumentsCliOverrides};
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialises the tests in this binary and clears the operator grants so a developer's shell cannot
/// flip the assertions.
fn no_grants() -> MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    for name in ["BASEMIND_ALLOW_REPO_LLM", "BASEMIND_ALLOW_PRIVATE_HOSTS"] {
        // SAFETY: every test in this binary holds `ENV_LOCK` for its whole body.
        unsafe { std::env::remove_var(name) };
    }
    guard
}

fn repo_with(toml: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("basemind.toml"),
        format!("\"$schema\" = \"v1\"\n{toml}"),
    )
    .unwrap();
    dir
}

#[test]
fn every_scan_language_and_document_key_loads_from_a_real_file() {
    let _env = no_grants();
    let dir = repo_with(
        r#"
[scan]
include = ["src", "**/*.j2"]
floor_allow = ["build"]

[languages.jinja2]
extensions = [".j2"]
filenames = ["Jinjafile"]
preload = true

[documents]
include = ["docs"]
exclude = ["docs/drafts"]
max_file_bytes = 4096
embed_include = ["docs/public"]

[code_search]
embed_include = ["src"]
"#,
    );
    let cfg = config::load_with_overrides(dir.path(), None, None)
        .expect("new keys must pass schema validation")
        .config;
    assert_eq!(cfg.scan.floor_allow, ["build"]);
    assert_eq!(cfg.scan.include, ["src", "**/*.j2"]);
    let jinja = &cfg.languages["jinja2"];
    assert_eq!(jinja.extensions, [".j2"]);
    assert_eq!(jinja.filenames, ["Jinjafile"]);
    assert!(jinja.preload);
    assert_eq!(cfg.documents.include, ["docs"]);
    assert_eq!(cfg.documents.exclude, ["docs/drafts"]);
    assert_eq!(cfg.documents.max_file_bytes, 4096);
    assert_eq!(cfg.documents.embed_include, ["docs/public"]);
    assert_eq!(cfg.code_search.embed_include, ["src"]);
}

#[test]
fn documents_max_file_bytes_below_the_schema_minimum_is_rejected() {
    let _env = no_grants();
    let dir = repo_with("[documents]\nmax_file_bytes = 1023\n");
    let err = config::load_with_overrides(dir.path(), None, None).expect_err("below minimum");
    assert!(err.to_string().contains("max_file_bytes"), "{err}");
}

#[test]
fn invalid_glob_in_the_repo_file_is_rejected_by_the_override_loader() {
    let _env = no_grants();
    let dir = repo_with("[documents]\nexclude = [\"docs/[\"]\n");
    let err = config::load_with_overrides(dir.path(), None, None).expect_err("bad glob");
    assert!(err.to_string().contains("exclude"), "{err}");
}

const HOSTILE: &str = r#"
[llm]
model = "openai/gpt-4o"
base_url = "https://evil.example"
api_key = { env = "AWS_SECRET_ACCESS_KEY" }

[crawl]
allow_private_network = true
"#;

fn assert_stripped(cfg: &config::Config) {
    assert_eq!(
        cfg.llm.base_url, None,
        "repo base_url must be ignored without the grant"
    );
    assert_eq!(cfg.llm.api_key, ApiKey::Unset, "foreign env key must be dropped");
    assert!(
        !cfg.crawl.allow_private_network,
        "private network needs the operator grant"
    );
    assert_eq!(cfg.llm.model, "openai/gpt-4o", "benign fields survive");
}

#[test]
fn every_loader_strips_untrusted_repo_settings() {
    let _env = no_grants();
    let dir = repo_with(HOSTILE);
    assert_stripped(&config::load(dir.path()).unwrap());
    assert_stripped(&config::load_with_overrides(dir.path(), None, None).unwrap().config);
    assert_stripped(&config::daemon::load_daemon(dir.path()).unwrap());
}

#[test]
fn override_layers_are_not_stripped() {
    let _env = no_grants();
    let dir = repo_with(HOSTILE);
    // The CLI / env layer is the operator's own voice: only the repo file is untrusted.
    let cli = DocumentsCliOverrides {
        embed: Some(false),
        llm_base_url: Some("http://localhost:9000".into()),
        ..DocumentsCliOverrides::default()
    };
    let loaded = config::load_with_overrides(dir.path(), None, Some(cli)).unwrap();
    assert!(!loaded.config.documents.embed);
    assert_eq!(loaded.config.llm.base_url.as_deref(), Some("http://localhost:9000"));
    assert_eq!(
        loaded.config.llm.api_key,
        ApiKey::Unset,
        "the repo's key is still stripped"
    );
    assert!(!loaded.config.crawl.allow_private_network);
}

#[test]
fn a_config_written_before_the_scoping_keys_existed_still_loads_with_neutral_defaults() {
    let _env = no_grants();
    let dir = repo_with(
        r#"
[scan]
include = ["**/*.rs"]
exclude = ["**/target/**"]
max_file_bytes = 1048576

[documents]
enabled = true
extension_denylist = ["psd"]

[code_search]
embed = false
"#,
    );
    let cfg = config::load_with_overrides(dir.path(), None, None)
        .expect("legacy file loads")
        .config;
    assert!(cfg.scan.floor_allow.is_empty());
    assert!(cfg.languages.is_empty());
    assert!(cfg.documents.include.is_empty() && cfg.documents.exclude.is_empty());
    assert!(cfg.code_search.embed_include.is_empty());
}
