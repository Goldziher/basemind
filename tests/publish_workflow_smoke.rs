//! Canary locking the `publish.yaml` ordering invariants so a later refactor cannot
//! silently let a registry publish run before the GitHub release is finalized, or let a
//! partial draft masquerade as a complete release. Post-mortem of the v0.22.1 release
//! window: the plugin/npm `latest` pointer and the crates.io push must only advance once
//! every platform asset exists and the release has been promoted — otherwise a downstream
//! clean install resolves a version whose binaries are not yet downloadable.
//!
//! These are structural assertions over the workflow text (no YAML dependency): they slice
//! a job's block and check the `needs` / `if` gating and the required-asset set within it.

use std::path::PathBuf;

fn workflow() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/publish.yaml");
    std::fs::read_to_string(&path).expect("read .github/workflows/publish.yaml")
}

/// The body of a top-level job: from its 2-space-indented `name:` header to the next
/// 2-space-indented job header. Steps live at deeper indentation, so the only 2-space
/// non-space lines are sibling job headers.
fn job_block<'a>(workflow: &'a str, job: &str) -> &'a str {
    let marker = format!("\n  {job}:\n");
    let start = workflow
        .find(&marker)
        .unwrap_or_else(|| panic!("job `{job}` not found in publish.yaml"))
        + 1;
    let rest = &workflow[start..];
    let after_header = rest.find('\n').map_or(rest.len(), |i| i + 1);
    let mut offset = after_header;
    for line in rest[after_header..].split_inclusive('\n') {
        let bytes = line.as_bytes();
        let is_job_header = bytes.len() >= 3
            && bytes[0] == b' '
            && bytes[1] == b' '
            && bytes[2] != b' '
            && bytes[2] != b'#';
        if is_job_header {
            return &rest[..offset];
        }
        offset += line.len();
    }
    rest
}

fn assert_gated_on_finalize(workflow: &str, job: &str) {
    let block = job_block(workflow, job);
    assert!(
        block.contains("finalize_release"),
        "job `{job}` must list finalize_release in `needs` so it cannot publish before the release finalizes",
    );
    assert!(
        block.contains("needs.finalize_release.result == 'success'"),
        "job `{job}` must gate its `if:` on finalize_release success",
    );
}

/// Every irreversible registry publish must wait for a finalized GitHub release.
#[test]
fn registry_publishes_wait_for_a_finalized_release() {
    let workflow = workflow();
    for job in [
        "publish_npm",
        "publish_opencode",
        "publish_pypi",
        "publish_pypi_hermes",
        "publish_crates",
    ] {
        assert_gated_on_finalize(&workflow, job);
    }
}

/// Promotion (which flips the release public and lets `latest` move) must require the full
/// platform-asset set plus checksums, not a partial draft.
#[test]
fn finalize_requires_the_full_asset_set() {
    let workflow = workflow();
    let block = job_block(&workflow, "finalize_release");
    for asset in [
        "basemind-x86_64-unknown-linux-gnu.tar.gz",
        "basemind-aarch64-unknown-linux-gnu.tar.gz",
        "basemind-aarch64-apple-darwin.tar.gz",
        "basemind-x86_64-apple-darwin.tar.gz",
        "basemind-x86_64-pc-windows-msvc.zip",
    ] {
        assert!(block.contains(asset), "finalize_release must require {asset} before promoting");
    }
    assert!(
        block.contains("_checksums.txt"),
        "finalize_release must require the checksums file before promoting",
    );
}

/// The "already published?" gate must count missing required assets, so a partial draft
/// reports incomplete and the build matrix reruns to heal it — the previous "any asset
/// present" check skipped the builders and a partial release could never complete.
#[test]
fn complete_release_detection_counts_the_full_set() {
    let workflow = workflow();
    let block = job_block(&workflow, "meta");
    assert!(
        block.contains("required=(") && block.contains("missing"),
        "meta must gate release_assets_exist on the full required asset set, not on any single asset",
    );
}

/// A publish run is never cancelled mid-flight: cargo publish and release promotion are
/// irreversible, so `cancel-in-progress` must be false.
#[test]
fn publish_is_never_cancelled_mid_flight() {
    assert!(
        workflow().contains("cancel-in-progress: false"),
        "publish concurrency must set cancel-in-progress: false",
    );
}
