//! Loads real `basemind.toml` files through the public entry points. The schema is the runtime
//! validator (`additionalProperties: false`), so a struct field added without regenerating it is
//! rejected here; and every loader must agree on what a repository file yields.

use std::fs;

use basemind::config::overrides::{DocumentsCliOverrides, load_with_overrides};
use basemind::config::{self, ApiKey, DocumentsConfig};
use tempfile::TempDir;

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
markdown_chunk_max_bytes = 2048
plain_text_chunk_max_bytes = 1024
embed_include = ["docs/public"]

[code_search]
embed_include = ["src"]
"#,
    );
    let cfg = load_with_overrides(dir.path(), None, None)
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
    assert_eq!(cfg.documents.markdown_chunk_max_bytes, 2048);
    assert_eq!(cfg.documents.plain_text_chunk_max_bytes, 1024);
    assert_eq!(cfg.documents.embed_include, ["docs/public"]);
    assert_eq!(cfg.code_search.embed_include, ["src"]);
}

#[test]
fn documents_max_file_bytes_below_the_schema_minimum_is_rejected() {
    let dir = repo_with("[documents]\nmax_file_bytes = 1023\n");
    let err = load_with_overrides(dir.path(), None, None).expect_err("below minimum");
    assert!(err.to_string().contains("max_file_bytes"), "{err}");
}

#[test]
fn documents_chunk_cutovers_default_and_reject_values_below_the_minimum() {
    let dir = repo_with("[documents]\nenabled = true\n");
    let cfg = load_with_overrides(dir.path(), None, None)
        .expect("defaults load")
        .config;
    assert_eq!(cfg.documents.markdown_chunk_max_bytes, 1024 * 1024);
    assert_eq!(cfg.documents.plain_text_chunk_max_bytes, 4 * 1024 * 1024);
    for key in ["markdown_chunk_max_bytes", "plain_text_chunk_max_bytes"] {
        let dir = repo_with(&format!("[documents]\n{key} = 1023\n"));
        let err = load_with_overrides(dir.path(), None, None).expect_err("below minimum");
        assert!(err.to_string().contains(key), "{err}");
    }
}

#[test]
fn invalid_glob_in_the_repo_file_is_rejected_by_the_override_loader() {
    let dir = repo_with("[documents]\nexclude = [\"docs/[\"]\n");
    let err = load_with_overrides(dir.path(), None, None).expect_err("bad glob");
    assert!(err.to_string().contains("exclude"), "{err}");
}

const REPO_LLM: &str = r#"
[llm]
model = "openai/gpt-4o"
base_url = "https://llm.example"
api_key = { env = "CORP_LLM_TOKEN" }
"#;

fn assert_llm_kept_and_deprecated(cfg: &config::Config) {
    assert_eq!(cfg.llm.model, "openai/gpt-4o");
    assert_eq!(cfg.llm.base_url.as_deref(), Some("https://llm.example"));
    assert_eq!(
        cfg.llm.api_key,
        ApiKey::Env {
            env: "CORP_LLM_TOKEN".into()
        }
    );
    assert_eq!(config::trust::deprecated_settings_set(cfg), vec!["llm"]);
}

#[test]
fn every_loader_keeps_a_repo_llm_table_and_lists_it_as_deprecated() {
    let dir = repo_with(REPO_LLM);
    assert_llm_kept_and_deprecated(&config::load(dir.path()).unwrap());
    assert_llm_kept_and_deprecated(&load_with_overrides(dir.path(), None, None).unwrap().config);
    assert_llm_kept_and_deprecated(&config::daemon::load_daemon(dir.path()).unwrap());
}

#[test]
fn override_layers_win_over_the_repo_file() {
    let dir = repo_with(REPO_LLM);
    let cli = DocumentsCliOverrides {
        embed: Some(false),
        llm_base_url: Some("http://localhost:9000".into()),
        ..DocumentsCliOverrides::default()
    };
    let loaded = load_with_overrides(dir.path(), None, Some(cli)).unwrap();
    assert!(!loaded.config.documents.embed);
    assert_eq!(loaded.config.llm.base_url.as_deref(), Some("http://localhost:9000"));
}

#[test]
fn a_config_written_before_the_scoping_keys_existed_still_loads_with_neutral_defaults() {
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
    let cfg = load_with_overrides(dir.path(), None, None)
        .expect("legacy file loads")
        .config;
    assert!(cfg.scan.floor_allow.is_empty());
    assert!(cfg.languages.is_empty());
    assert!(cfg.documents.include.is_empty());
    assert_eq!(cfg.documents.exclude, DocumentsConfig::default().exclude);
    assert!(cfg.code_search.embed_include.is_empty());
}

/// Every loader, so a table for a removed feature cannot be accepted by one and rejected by another.
fn assert_loads_everywhere(toml: &str) -> config::Config {
    let dir = repo_with(toml);
    config::daemon::load_daemon(dir.path()).expect("load_daemon");
    load_with_overrides(dir.path(), None, None).expect("load_with_overrides");
    config::load(dir.path()).expect("load")
}

#[test]
fn a_crawl_table_written_for_an_older_version_still_loads_and_is_ignored() {
    let empty = assert_loads_everywhere("[crawl]\n");
    assert!(empty.crawl.is_empty(), "`basemind init` used to write this empty table");

    let cfg = assert_loads_everywhere(
        r#"
[crawl]
respect_robots_txt = false
max_pages = 64
user_agent = "docs-bot/1.0"
allow_private_network = true
no_such_key = { nested = [1, 2] }
"#,
    );
    assert!(!cfg.crawl.is_empty());
    assert_eq!(config::trust::deprecated_sections_set(&cfg), vec!["crawl"]);
}

#[test]
fn a_shells_table_written_for_an_older_version_still_loads_and_is_ignored() {
    let cfg = assert_loads_everywhere(
        r#"
[shells]
enabled = true
visual = "window"
terminal = "ghostty"
keep_on_exit = true
no_such_key = { nested = [1, 2] }
"#,
    );
    assert!(!cfg.shells.is_empty());
    assert!(cfg.crawl.is_empty());
    assert_eq!(config::trust::deprecated_sections_set(&cfg), vec!["shells"]);
}

#[test]
fn an_unknown_table_is_still_rejected() {
    let dir = repo_with("[crawler]\nmax_pages = 64\n");
    let err = config::load(dir.path())
        .expect_err("unknown top-level table")
        .to_string();
    assert!(err.contains("crawler"), "{err}");
    load_with_overrides(dir.path(), None, None).expect_err("unknown top-level table");
    config::daemon::load_daemon(dir.path()).expect_err("unknown top-level table");
}

#[test]
fn settings_for_removed_document_capabilities_still_load_and_are_listed_as_deprecated() {
    let cfg = assert_loads_everywhere(
        r#"
[documents]
extract_archives = true

[documents.summarization]
enabled = true
strategy = "abstractive"

[documents.ner]
enabled = true
backend = "llm"

[llm]
model = "openai/gpt-4o"
api_key = { env = "OPENAI_API_KEY" }
temperature = 0.2
timeout_secs = 30
max_retries = 2
max_tokens = 512
"#,
    );
    assert_eq!(
        config::trust::deprecated_settings_set(&cfg),
        vec!["documents.extract_archives", "documents.summarization.strategy", "llm"]
    );
    assert!(config::trust::deprecated_settings_set(&config::ConfigV1::with_defaults()).is_empty());
    assert!(config::trust::deprecated_sections_set(&cfg).is_empty());
}

#[test]
fn an_llm_override_flag_is_accepted_and_listed_as_deprecated() {
    let dir = repo_with("");
    let overrides = DocumentsCliOverrides {
        llm_model: Some("openai/gpt-4o".to_string()),
        ..DocumentsCliOverrides::default()
    };
    let loaded = load_with_overrides(dir.path(), None, Some(overrides)).expect("load");
    assert_eq!(config::trust::deprecated_settings_set(&loaded.config), vec!["llm"]);
}
