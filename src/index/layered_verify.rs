use anyhow::{Result, ensure};

use super::view::{IndexView, Part};

/// Verify the persisted row contract before publishing an overlay for readers.
pub(super) fn verify_rows(layered: &dyn IndexView, standalone: &dyn IndexView) -> Result<()> {
    let mut visited = 0;
    for part in Part::ALL
        .into_iter()
        .filter(|part| !matches!(part, Part::Meta | Part::GrepBloom))
    {
        let mut layered = layered.iter(part);
        let mut standalone = standalone.iter(part);
        let mut row = 0;
        loop {
            let left = layered.next().transpose()?;
            let right = standalone.next().transpose()?;
            ensure!(left == right, "overlay mismatch in {} at row {row}", part.name());
            if left.is_none() {
                break;
            }
            row += 1;
        }
        visited += row;
    }
    // Bloom rows are content-addressed and the base retains rows for retired hashes.
    for row in standalone.iter(Part::GrepBloom) {
        let (key, value) = row?;
        ensure!(
            layered.get(Part::GrepBloom, &key)? == Some(value),
            "overlay bloom row differs"
        );
        visited += 1;
    }
    ensure!(visited > 0, "verification compared no index rows");
    ensure!(
        layered.bm25_stats() == standalone.bm25_stats(),
        "overlay BM25 totals differ"
    );
    ensure!(
        layered.import_probes_complete() == standalone.import_probes_complete(),
        "overlay import probe completeness differs"
    );
    Ok(())
}

#[test]
fn verifies_candidate_only_overlay_for_a_real_git_worktree() {
    use ahash::AHashMap;

    use super::layered_shadow::DeltaManifest;
    use super::layered_view::LayeredView;
    use crate::config::Config;
    use crate::git::Repo;
    use crate::scanner::{EmbedMode, ScanSource, scan, scan_paths};
    use crate::store::Store;

    fn git(root: &std::path::Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    crate::store::init_isolated_cache();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    for (name, text) in [("edit.rs", "fn a() { run(); }"), ("keep.rs", "fn b() { run(); }")] {
        std::fs::write(root.join(name), text).unwrap();
    }
    git(root, &["add", "edit.rs", "keep.rs"]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    let mut config = Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    let mut base = Store::open(root, "working").unwrap();
    scan(root, &mut base, &config, ScanSource::WorkingTree, EmbedMode::Deferred).unwrap();
    let base_files = base.index.files.clone();
    let base_view_path = base.view_dir.clone();
    drop(base);
    std::fs::write(root.join("edit.rs"), "fn a() { run(); run(); }").unwrap();
    std::fs::write(root.join("new.rs"), "fn c() { run(); }").unwrap();

    let repo = Repo::discover(root).unwrap();
    let commit = repo.resolve_rev("HEAD").unwrap();
    let candidates = repo.status_against(&commit).unwrap().overlay_candidates();
    assert_eq!(candidates.len(), 2);
    let paths = candidates
        .iter()
        .map(|path| root.join(path.to_path_buf()))
        .collect::<Vec<_>>();
    let mut overlay = Store::open(root, "overlay-test").unwrap();
    scan_paths(root, &mut overlay, &config, &paths, EmbedMode::Deferred).unwrap();
    assert_eq!(overlay.index.files.len(), 2);

    let overlay_files = overlay.index.files.clone();
    let overlay_view_path = overlay.view_dir.clone();
    drop(overlay);
    let mut reference = Store::open(root, "reference-test").unwrap();
    scan(
        root,
        &mut reference,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .unwrap();
    assert_eq!(reference.index.files.len(), 3);
    let mut counts = AHashMap::new();
    for file in base_files.values() {
        *counts.entry(file.hash_hex.as_str()).or_default() += 1;
    }
    let mut delta = DeltaManifest::empty(1, commit, 1);
    for path in candidates {
        if let Some(entry) = overlay_files.get(&path) {
            delta.upsert(path.clone(), entry.clone(), base_files.get(&path));
        } else if base_files.get(&path).is_some() {
            delta.delete(path);
        }
    }
    let shadow = delta.shadow(&counts, |path| base_files.get(path));
    let base = super::IndexDb::open(&base_view_path).unwrap();
    let overlay = super::IndexDb::open(&overlay_view_path).unwrap();
    let layered = LayeredView::new(&base, &overlay, shadow).unwrap();
    verify_rows(&layered, &reference.index_view().unwrap()).unwrap();
}
