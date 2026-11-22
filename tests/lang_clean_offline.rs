//! `basemind lang clean` while grammars are offline: it refuses without `--yes` and leaves a
//! hand-copied grammar cache alone.

use std::process::Command;

fn clean(cache: &std::path::Path, home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["lang", "clean"])
        .args(args)
        .env("BASEMIND_OFFLINE", "1")
        .env("BASEMIND_DATA_HOME", home)
        .env("TREE_SITTER_LANGUAGE_PACK_CACHE_DIR", cache)
        .output()
        .expect("run basemind lang clean")
}

/// The grammar cache directory the binary resolves under `cache`, which is a subdirectory of it.
fn grammar_dir(cache: &std::path::Path, home: &std::path::Path) -> std::path::PathBuf {
    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["lang", "list"])
        .env("BASEMIND_OFFLINE", "1")
        .env("BASEMIND_DATA_HOME", home)
        .env("TREE_SITTER_LANGUAGE_PACK_CACHE_DIR", cache)
        .output()
        .expect("run basemind lang list");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let dir = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("cache: "))
        .unwrap_or_else(|| panic!("no cache line in {stdout}"));
    std::path::PathBuf::from(dir)
}

#[test]
fn offline_lang_clean_refuses_without_yes_and_deletes_with_it() {
    let cache = tempfile::tempdir().expect("cache dir");
    let home = tempfile::tempdir().expect("data home");
    let grammars = grammar_dir(cache.path(), home.path());
    assert!(grammars.starts_with(cache.path()), "{grammars:?}");
    std::fs::create_dir_all(&grammars).unwrap();
    let marker = grammars.join("grammar.marker");
    std::fs::write(&marker, b"copied by hand").unwrap();

    let refused = clean(cache.path(), home.path(), &[]);
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--yes"),
        "{refused:?}"
    );
    assert!(marker.exists(), "the refused clean left the cache alone");

    let forced = clean(cache.path(), home.path(), &["--yes"]);
    assert!(forced.status.success(), "{forced:?}");
    assert!(!marker.exists(), "--yes cleared the cache");
}
