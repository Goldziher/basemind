//! `scan.extra_roots` containment: a repository cannot grant itself indexing authority over
//! directories outside its own tree.
//!
//! `extra_roots` is read from `<repo>/basemind.toml` — a file inside the tree being scanned, so a
//! cloned repository authors it. Without an operator-side grant, merely indexing such a clone used
//! to walk whatever it named (`~/.ssh`, `~/.aws`), follow symlinks out of it, and surface the
//! contents through the agent-facing search tools.
//!
//! This suite deliberately lives in its own test binary, and never touches the grant: the grant
//! (`basemind::scanner::allow_extra_roots`) is process-wide, and `tests/extra_roots_smoke.rs` turns
//! it on for every test in *its* process — which is also where the granted path is covered, so
//! that the containment cannot be mistaken for the feature simply being broken. Merging the two
//! would make these assertions depend on test ordering.

use std::fs;

use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

/// The attack, end to end: a repository whose committed `basemind.toml` points `extra_roots` at a
/// directory of secrets the operator never named, loaded through the real config loader rather
/// than assembled in memory.
#[test]
fn repo_supplied_extra_roots_cannot_index_an_out_of_repo_directory() {
    basemind::store::init_isolated_cache();
    let repo = tempfile::tempdir().expect("repo tempdir");
    let secrets = tempfile::tempdir().expect("secrets tempdir");

    fs::write(secrets.path().join("id_rsa.rs"), b"pub fn private_key() {}\n").unwrap();
    fs::write(repo.path().join("main.rs"), b"fn main() {}\n").unwrap();
    let secrets_path = fs::canonicalize(secrets.path()).unwrap();
    fs::write(
        repo.path().join("basemind.toml"),
        format!(
            "\"$schema\" = \"v1\"\n[scan]\ninclude = [\"**\"]\nextra_roots = [\"{}\"]\n",
            secrets_path.display()
        ),
    )
    .unwrap();

    let cfg = basemind::config::load(repo.path()).expect("the repo's own config parses");
    assert_eq!(cfg.scan.extra_roots, vec![secrets_path.clone()], "config was honored");

    let mut store = Store::open(repo.path(), VIEW_WORKING).unwrap();
    scan(
        repo.path(),
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .expect("an ungranted extra root is ignored, not a scan failure");

    let secret_key = secrets_path.join("id_rsa.rs").to_str().unwrap().to_string();
    #[cfg(windows)]
    let secret_key = secret_key.replace('\\', "/");
    assert!(
        store.lookup(secret_key.as_bytes()).is_none(),
        "a repo-authored extra_roots entry must not pull an out-of-repo tree into the index"
    );
    assert!(store.lookup("main.rs").is_some(), "the repository itself still indexes");
    assert!(
        basemind::query::search_symbols(&store, "private_key", None)
            .unwrap()
            .is_empty(),
        "and nothing from that tree is reachable through the search tools"
    );
}
