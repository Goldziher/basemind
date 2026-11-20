//! A symbol named in the working tree is looked up in the file as it is now, not in whatever the
//! index holds for it: the watcher rescans after a commit, so a call can land while the index has
//! no entry for the file yet, or one for older content.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use super::*;
use crate::path::RelPath;

const STEADY_RS: &str = "pub fn stable() -> u32 {\n    7\n}\n\npub fn churn() -> u32 {\n    0\n}\n";

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// A repository whose first commit holds `src/steady.rs`, and a workspace over it that has
/// scanned nothing: the store has no entry for the file, as between a commit and its rescan.
fn unscanned(root: &Path, cache: &Path) -> Workspace {
    committed(root, cache, &[], &[("src/steady.rs", STEADY_RS)])
}

/// A repository with `config` set in its own config and a first commit holding `files`, and a
/// workspace over it that has scanned nothing.
fn committed(root: &Path, cache: &Path, config: &[(&str, &str)], files: &[(&str, &str)]) -> Workspace {
    crate::store::init_isolated_cache();
    let _ = crate::lang::ensure_grammars().expect("grammar bootstrap");
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for &(key, value) in config {
        git(root, &["config", key, value]);
    }
    for &(name, content) in files {
        let file = root.join(name);
        std::fs::create_dir_all(file.parent().expect("parent")).unwrap();
        std::fs::write(file, content).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    workspace_over(root, cache)
}

/// [`unscanned`] after a scan: the index describes the first commit's `src/steady.rs`.
fn scanned(root: &Path, cache: &Path) -> Workspace {
    drop(unscanned(root, cache));
    let mut store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("store");
    crate::scanner::scan(
        root,
        &mut store,
        &crate::config::default_for_root(root),
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
        crate::retrieval_wiring::backend(),
    )
    .expect("scan");
    assert!(
        store.lookup(RelPath::from("src/steady.rs")).is_some(),
        "the scan indexed the file"
    );
    drop(store);
    workspace_over(root, cache)
}

fn workspace_over(root: &Path, cache: &Path) -> Workspace {
    let store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("store");
    let repo = crate::git::Repo::discover(root).expect("repo");
    Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        Some(Arc::new(repo)),
        Arc::new(crate::git_cache::GitCache::open(cache, 4, true).expect("git cache")),
        None,
        crate::engine::workspace::OpenOptions {
            background: true,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            #[cfg(feature = "memory")]
            memory_owner: |_| "owner".to_string(),
        },
    )
}

fn history(ws: &Workspace, name: &str) -> Result<SymbolHistoryResponse, EngineError> {
    run_symbol_history(
        ws,
        SymbolHistoryParams {
            path: RelPath::from("src/steady.rs"),
            name: name.to_string(),
            kind: None,
            line: None,
            limit: None,
            hash_mode: None,
            max_tokens: None,
            cursor: None,
        },
    )
}

fn changes(response: &SymbolHistoryResponse) -> Vec<(&'static str, &str)> {
    response
        .history
        .iter()
        .map(|entry| (entry.change, entry.summary.as_str()))
        .collect()
}

#[test]
fn a_mistyped_symbol_in_a_file_not_yet_indexed_lists_the_live_file_s_symbols() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let ws = unscanned(repo_dir.path(), cache_dir.path());

    let error = history(&ws, "stabel")
        .expect_err("no commit defines `stabel`")
        .to_string();
    assert!(error.contains("no symbol `stabel` in `src/steady.rs`"), "{error}");
    assert!(
        error.contains("it defines:") && error.contains("stable") && error.contains("churn"),
        "the candidates are the working tree's: {error}"
    );

    let stable = history(&ws, "stable").expect("`stable` is defined");
    assert_eq!(changes(&stable), [("introduced", "init")]);
    assert_eq!(stable.unmatched, None);
}

#[test]
fn the_working_tree_outline_is_the_file_as_it_is_now() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let ws = unscanned(root, cache_dir.path());
    std::fs::write(
        root.join("src/steady.rs"),
        format!("{STEADY_RS}\npub fn fresh() {{}}\n"),
    )
    .unwrap();
    git(root, &["commit", "-qam", "add fresh"]);

    let fresh = history(&ws, "fresh").expect("`fresh` is defined");
    assert_eq!(changes(&fresh), [("introduced", "add fresh")]);
    let error = history(&ws, "frsh").expect_err("a typo").to_string();
    assert!(error.contains("fresh"), "{error}");
}

#[test]
fn a_structural_diff_compares_the_file_as_it_is_now() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let ws = unscanned(root, cache_dir.path());
    std::fs::write(
        root.join("src/steady.rs"),
        format!("{STEADY_RS}\npub fn fresh() {{}}\n"),
    )
    .unwrap();

    let structural = run_diff_outline(
        &ws,
        DiffOutlineParams {
            path: RelPath::from("src/steady.rs"),
            rev: None,
        },
    )
    .expect("diff outline");
    let added: Vec<&str> = structural.added.iter().map(|symbol| symbol.name.as_str()).collect();
    assert_eq!(added, ["fresh"], "{:?}", structural.note);
    assert!(structural.removed.is_empty());
    assert_eq!(structural.common, 2);
    assert_eq!(structural.note, None);
}

/// The structural diff of `src/steady.rs` against HEAD as its added and removed names, its
/// common count and its note.
fn structural_against_head(ws: &Workspace) -> (Vec<String>, Vec<String>, usize, Option<String>) {
    let diff = run_diff_outline(
        ws,
        DiffOutlineParams {
            path: RelPath::from("src/steady.rs"),
            rev: None,
        },
    )
    .expect("diff outline");
    let names =
        |symbols: Vec<DiffSymbolView>| -> Vec<String> { symbols.into_iter().map(|symbol| symbol.name).collect() };
    (names(diff.added), names(diff.removed), diff.common, diff.note)
}

#[test]
fn a_structural_diff_of_a_crlf_checkout_under_text_auto_is_unchanged() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let ws = committed(
        root,
        cache_dir.path(),
        &[],
        &[(".gitattributes", "* text=auto\n"), ("src/steady.rs", STEADY_RS)],
    );
    std::fs::write(root.join("src/steady.rs"), STEADY_RS.replace('\n', "\r\n")).unwrap();

    assert_eq!(structural_against_head(&ws), (Vec::new(), Vec::new(), 2, None));
}

/// The fake driver stands in for Git LFS: git stores `pointer` where the working tree holds
/// `payload`, so the working tree's names differ from the blob's until it is in its git form.
#[test]
fn a_structural_diff_of_a_clean_filtered_file_compares_its_git_form() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let ws = committed(
        root,
        cache_dir.path(),
        &[("filter.fake.clean", "sed s/payload/pointer/")],
        &[
            (".gitattributes", "*.rs filter=fake\n"),
            ("src/steady.rs", "pub fn payload() {}\n"),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/steady.rs")).unwrap(),
        "pub fn payload() {}\n",
        "the working tree keeps the unfiltered text"
    );

    assert_eq!(structural_against_head(&ws), (Vec::new(), Vec::new(), 1, None));
}

#[test]
fn a_file_the_working_tree_no_longer_has_still_has_a_symbol_history() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let ws = unscanned(root, cache_dir.path());
    git(root, &["rm", "-q", "src/steady.rs"]);
    git(root, &["commit", "-qm", "drop steady"]);

    let stable = history(&ws, "stable").expect("`stable` has a history");
    assert_eq!(changes(&stable), [("removed", "drop steady"), ("introduced", "init")]);
    let never = history(&ws, "never_defined")
        .expect_err("no commit defines it")
        .to_string();
    assert!(never.contains("no symbol `never_defined`"), "{never}");
}

#[test]
fn a_working_tree_file_over_the_scan_limit_is_refused_with_the_fix() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let ws = unscanned(root, cache_dir.path());
    let max = ws.config.scan.max_file_bytes as usize;
    std::fs::write(root.join("src/steady.rs"), format!("{STEADY_RS}{}", "/".repeat(max))).unwrap();

    let error = history(&ws, "stable").expect_err("too large to parse").to_string();
    assert!(error.contains("`scan.max_file_bytes`"), "{error}");
}

#[test]
fn an_indexed_file_is_judged_by_its_live_content() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let ws = scanned(root, cache_dir.path());

    let error = history(&ws, "stabel").expect_err("a typo").to_string();
    assert!(error.contains("stable") && error.contains("churn"), "{error}");

    // Edited after the scan, not rescanned: the candidates are the file's now, not the index's.
    std::fs::write(root.join("src/steady.rs"), STEADY_RS.replace("churn", "churned")).unwrap();
    let error = history(&ws, "churnd").expect_err("a typo").to_string();
    assert!(error.contains("churned"), "{error}");
    let renamed = history(&ws, "churned").expect("defined in the working tree");
    assert_eq!(changes(&renamed), Vec::<(&str, &str)>::new(), "no commit has it yet");
    assert_eq!(renamed.unmatched, None, "the working tree defines it");
}
