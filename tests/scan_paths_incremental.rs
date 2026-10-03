//! The incremental path (`scan_paths`, driven by the watcher and `rescan paths=[..]`) must index
//! exactly what a full scan would: no symlinked files or directories unless `follow_symlinks` is on,
//! and an existing file that stopped being eligible is evicted rather than left stale.

use std::fs;
use std::path::{Path, PathBuf};

use basemind::config::{ConfigV1, LanguageConfig};
use basemind::scanner::{EmbedMode, ScanSource, scan, scan_paths};
use basemind::store::{Store, VIEW_WORKING};
use tempfile::TempDir;

const PY: &str = "def leaked_symbol():\n    return 1\n";

fn repo() -> (TempDir, ConfigV1) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    (dir, cfg)
}

fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn incremental(root: &Path, store: &mut Store, cfg: &ConfigV1, rels: &[&str]) -> basemind::scanner::ScanReport {
    let abs: Vec<PathBuf> = rels.iter().map(|r| root.join(r)).collect();
    scan_paths(root, store, cfg, &abs, EmbedMode::Inline).expect("scan_paths")
}

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A repo with a tracked file link and a directory link that both point outside the root.
    fn linked_repo() -> (TempDir, TempDir, ConfigV1) {
        let (dir, cfg) = repo();
        let outside = tempfile::tempdir().expect("outside");
        write(outside.path(), "secret.py", PY);
        write(outside.path(), "deep/other.py", PY);
        write(dir.path(), "real.py", "def real_symbol():\n    return 2\n");
        symlink(outside.path().join("secret.py"), dir.path().join("leak.py")).unwrap();
        symlink(outside.path().join("deep"), dir.path().join("linkdir")).unwrap();
        (dir, outside, cfg)
    }

    #[test]
    fn incremental_scan_does_not_follow_symlinks_by_default() {
        let (dir, _outside, cfg) = linked_repo();
        let root = dir.path();
        let mut store = Store::open(root, VIEW_WORKING).unwrap();

        let report = incremental(root, &mut store, &cfg, &["real.py", "leak.py", "linkdir/other.py"]);

        assert!(
            store.lookup("real.py").is_some(),
            "the regular file is indexed (control)"
        );
        assert!(store.lookup("leak.py").is_none(), "file symlink must be skipped");
        assert!(
            store.lookup("linkdir/other.py").is_none(),
            "a path through a directory symlink must be skipped"
        );
        assert_eq!(report.stats.scanned, 1, "only the regular file reaches the pipeline");
    }

    #[test]
    fn incremental_scan_agrees_with_the_full_scan_on_symlinks() {
        let (dir, _outside, cfg) = linked_repo();
        let root = dir.path();
        let mut full = Store::open(root, VIEW_WORKING).unwrap();
        scan(root, &mut full, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
        assert!(full.lookup("real.py").is_some(), "full scan indexes the regular file");
        assert!(full.lookup("leak.py").is_none(), "full scan skips the symlink");
    }

    #[test]
    fn incremental_scan_follows_symlinks_when_the_config_opts_in() {
        let (dir, _outside, mut cfg) = linked_repo();
        cfg.scan.follow_symlinks = true;
        let root = dir.path();
        let mut store = Store::open(root, VIEW_WORKING).unwrap();

        incremental(root, &mut store, &cfg, &["leak.py", "linkdir/other.py"]);

        assert!(store.lookup("leak.py").is_some());
        assert!(store.lookup("linkdir/other.py").is_some());
    }

    #[test]
    fn an_already_indexed_file_replaced_by_a_symlink_is_evicted() {
        let (dir, outside, cfg) = linked_repo();
        let root = dir.path();
        let mut store = Store::open(root, VIEW_WORKING).unwrap();
        incremental(root, &mut store, &cfg, &["real.py"]);
        assert!(store.lookup("real.py").is_some());

        fs::remove_file(root.join("real.py")).unwrap();
        symlink(outside.path().join("secret.py"), root.join("real.py")).unwrap();
        incremental(root, &mut store, &cfg, &["real.py"]);

        assert!(
            store.lookup("real.py").is_none(),
            "the swapped-in link must not stay indexed"
        );
    }
}

#[test]
fn disabling_a_language_evicts_its_file_from_an_incremental_scan() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "a.py", PY);
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    incremental(root, &mut store, &cfg, &["a.py"]);
    assert!(store.lookup("a.py").is_some());

    cfg.languages.insert(
        "python".to_string(),
        LanguageConfig {
            enabled: false,
            ..LanguageConfig::default()
        },
    );
    incremental(root, &mut store, &cfg, &["a.py"]);

    assert!(store.lookup("a.py").is_none(), "disabled language must drop the entry");
}

#[test]
fn a_file_grown_past_the_size_cap_is_evicted_from_an_incremental_scan() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "big.py", PY);
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    incremental(root, &mut store, &cfg, &["big.py"]);
    assert!(store.lookup("big.py").is_some());

    cfg.scan.max_file_bytes = 8;
    incremental(root, &mut store, &cfg, &["big.py"]);

    assert!(store.lookup("big.py").is_none());
}

#[test]
fn a_newly_excluded_file_is_evicted_from_an_incremental_scan() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "gen/a.py", PY);
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    incremental(root, &mut store, &cfg, &["gen/a.py"]);
    assert!(store.lookup("gen/a.py").is_some());

    cfg.scan.exclude.push("gen".to_string());
    incremental(root, &mut store, &cfg, &["gen/a.py"]);

    assert!(store.lookup("gen/a.py").is_none());
}
