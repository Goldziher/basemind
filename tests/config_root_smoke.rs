//! Smoke tests for the root-relocated config: `basemind.toml` at the repo root is the canonical
//! location, the legacy `.basemind/basemind.toml` is still read as a fallback, and `basemind init`
//! scaffolds the root file and appends a `.gitignore` pattern covering the personal `.local` rule
//! file it writes (the index cache itself is machine-global and out-of-repo). Also covers the new
//! scan/embed config fields introduced alongside the relocation (`follow_symlinks`, `embed_exclude`,
//! `extract_archives`, and `code_search.embed` defaulting off).

use std::fs;
use std::process::Command;

use basemind::config;

/// Write `contents` to `path`, creating parent dirs as needed.
fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, contents).expect("write file");
}

#[test]
fn load_reads_root_basemind_toml() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(
        &config::config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 4096\n",
    );
    let cfg = config::load(root).expect("root config loads");
    assert_eq!(cfg.scan.max_file_bytes, 4096);
}

#[test]
fn load_falls_back_to_legacy_in_cache_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(
        &config::legacy_config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 8192\n",
    );
    let cfg = config::load(root).expect("legacy config loads");
    assert_eq!(cfg.scan.max_file_bytes, 8192);
}

#[test]
fn root_config_wins_over_legacy_when_both_present() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(
        &config::config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 4096\n",
    );
    write(
        &config::legacy_config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 8192\n",
    );
    let cfg = config::load(root).expect("config loads");
    assert_eq!(cfg.scan.max_file_bytes, 4096, "root basemind.toml must win over legacy");
}

#[test]
fn load_reads_the_flat_config_convention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(
        &config::convention_flat_config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 2048\n",
    );
    let cfg = config::load(root).expect("flat .config convention loads");
    assert_eq!(cfg.scan.max_file_bytes, 2048);
}

#[test]
fn load_reads_the_nested_config_convention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(
        &config::convention_nested_config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 1024\n",
    );
    let cfg = config::load(root).expect("nested .config convention loads");
    assert_eq!(cfg.scan.max_file_bytes, 1024);
}

#[test]
fn root_config_wins_over_the_config_convention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(
        &config::config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 4096\n",
    );
    write(
        &config::convention_flat_config_path(root),
        "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 9999\n",
    );
    let cfg = config::load(root).expect("config loads");
    assert_eq!(
        cfg.scan.max_file_bytes, 4096,
        "the root basemind.toml must win over the .config convention"
    );
}

#[test]
fn init_config_dir_scaffolds_the_flat_convention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["init", "--yes", "--config-dir", ".config"])
        .current_dir(root)
        .status()
        .expect("run basemind init");
    assert!(status.success());

    assert!(
        config::convention_flat_config_path(root).exists(),
        "init --config-dir .config writes .config/basemind.toml"
    );
    assert!(
        !config::config_path(root).exists(),
        "no root basemind.toml when the convention is requested"
    );
    let cfg = config::load(root).expect("scaffolded convention config loads + validates");
    assert_eq!(cfg.schema, "v1");
}

#[test]
fn init_config_dir_scaffolds_the_nested_convention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["init", "--yes", "--config-dir", ".config/basemind"])
        .current_dir(root)
        .status()
        .expect("run basemind init");
    assert!(status.success());

    assert!(
        config::convention_nested_config_path(root).exists(),
        "init --config-dir .config/basemind writes .config/basemind/config.toml"
    );
    assert!(config::load(root).is_ok(), "nested convention config loads");
}

#[test]
fn init_rejects_a_config_dir_outside_the_convention() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let out = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["init", "--yes", "--config-dir", "some/other/dir"])
        .current_dir(root)
        .output()
        .expect("run basemind init");
    assert!(!out.status.success(), "a non-convention config dir is refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported --config-dir"),
        "error explains the supported values, got: {stderr:?}"
    );
}

#[test]
fn code_search_embed_defaults_off() {
    let cfg = config::parse_str("\"$schema\" = \"v1\"\n").expect("minimal config parses");
    assert!(!cfg.code_search.embed, "code_search.embed must default to false");
    assert!(cfg.documents.embed, "documents.embed must default to true");
}

#[test]
fn follow_symlinks_field_parses_and_defaults_false() {
    let default_cfg = config::parse_str("\"$schema\" = \"v1\"\n").expect("parse");
    assert!(!default_cfg.scan.follow_symlinks, "follow_symlinks defaults to false");
    let cfg =
        config::parse_str("\"$schema\" = \"v1\"\n[scan]\nfollow_symlinks = true\n").expect("follow_symlinks parses");
    assert!(cfg.scan.follow_symlinks);
}

#[test]
fn extract_archives_toggle_parses_and_defaults_false() {
    let default_cfg = config::parse_str("\"$schema\" = \"v1\"\n").expect("parse");
    assert!(
        !default_cfg.documents.extract_archives,
        "extract_archives defaults to false"
    );
    let cfg = config::parse_str("\"$schema\" = \"v1\"\n[documents]\nextract_archives = true\n")
        .expect("extract_archives parses");
    assert!(cfg.documents.extract_archives);
}

#[test]
fn embed_exclude_parses_on_both_tiers() {
    let cfg = config::parse_str(
        "\"$schema\" = \"v1\"\n\
         [documents]\nembed_exclude = [\"**/generated/**\"]\n\
         [code_search]\nembed_exclude = [\"**/vendor/**\"]\n",
    )
    .expect("embed_exclude parses on both tiers");
    assert_eq!(cfg.documents.embed_exclude, vec!["**/generated/**".to_string()]);
    assert_eq!(cfg.code_search.embed_exclude, vec!["**/vendor/**".to_string()]);
}

#[test]
fn init_writes_root_config_and_gitignores_the_local_rule_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["init", "--yes"])
        .current_dir(root)
        .status()
        .expect("run basemind init");
    assert!(status.success(), "basemind init exits successfully");

    let config_path = config::config_path(root);
    assert!(config_path.exists(), "basemind init writes root basemind.toml");
    let cfg = config::load(root).expect("scaffolded config loads + validates");
    assert_eq!(cfg.schema, "v1");

    // ~keep The index cache lives in the machine-global XDG cache, but init still writes a personal
    // ~keep `.local` rule file, so it appends the `.gitignore` pattern that covers it.
    let gitignore = fs::read_to_string(root.join(".gitignore")).expect("init writes a .gitignore");
    assert_eq!(gitignore, "CLAUDE.local.md\n", "the local rule file is gitignored");
}

#[test]
fn init_refuses_to_shadow_a_legacy_in_cache_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let legacy = config::legacy_config_path(root);
    fs::create_dir_all(legacy.parent().expect("legacy parent")).expect("mkdir .basemind");
    fs::write(
        &legacy,
        "\"$schema\" = \"v1\"\n\n[scan]\nexclude = [\"**/secret/**\"]\n",
    )
    .expect("seed legacy");

    let out = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .arg("init")
        .current_dir(root)
        .output()
        .expect("run basemind init");
    assert!(
        !out.status.success(),
        "init must fail rather than shadow a legacy config"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("legacy config"),
        "error names the legacy config, got: {stderr:?}"
    );

    assert!(
        !config::config_path(root).exists(),
        "no root config written when legacy exists"
    );
    let cfg = config::load(root).expect("legacy config still loads");
    assert_eq!(
        cfg.scan.exclude,
        vec!["**/secret/**".to_string()],
        "legacy setting preserved"
    );
}

#[test]
fn init_preserves_an_existing_gitignore_and_appends_the_local_pattern() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join(".gitignore"), "target/\n").expect("seed gitignore");

    let ok = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["init", "--yes"])
        .current_dir(root)
        .status()
        .expect("run init");
    assert!(ok.success());
    // ~keep init writes no in-repo cache entry (the cache is out-of-repo), but it does add the
    // ~keep pattern for the personal `.local` rule file it writes — existing content stays verbatim.
    let after = fs::read_to_string(root.join(".gitignore")).expect("read gitignore");
    assert_eq!(
        after, "target/\nCLAUDE.local.md\n",
        "existing content preserved, the local pattern appended"
    );
}
