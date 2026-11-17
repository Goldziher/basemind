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

fn read_workflow(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read .github/workflows/{name}: {e}"))
        .replace("\r\n", "\n")
}

fn workflow() -> String {
    read_workflow("publish.yaml")
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
        let is_job_header =
            bytes.len() >= 3 && bytes[0] == b' ' && bytes[1] == b' ' && bytes[2] != b' ' && bytes[2] != b'#';
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

/// A release that cannot reach crates.io fails before anything publishes. v0.33.0 skipped crates.io
/// for a Git-pinned dependency with a green run; every other registry shipped it.
#[test]
fn a_crates_io_blocker_fails_the_release_instead_of_skipping_it() {
    let workflow = workflow();
    let meta = job_block(&workflow, "meta");
    let publish = job_block(&workflow, "publish_crates");
    assert!(
        meta.contains("git[[:space:]]*=") && meta.contains("exit 1") && !meta.contains("crates_publishable"),
        "meta must fail, not skip, on a Git-pinned dependency",
    );
    assert!(
        meta.contains("scripts/check_vendored_crates.py"),
        "meta must refuse vendored crates that drifted from their published version",
    );
    assert!(
        !publish.contains("crates_publishable"),
        "publish_crates must not be skippable for publishability"
    );
    assert!(
        publish.contains("publish basemind --features full"),
        "the basemind publish must verify the `full` build against the registry copies of its forks",
    );
}

/// Nothing publishes for a commit CI has not passed.
#[test]
fn publish_waits_for_a_green_ci_run() {
    let workflow = workflow();
    let gate = job_block(&workflow, "ci_gate");
    assert!(
        gate.contains(".github/scripts/require-green-ci.sh") && gate.contains("actions: read"),
        "ci_gate must run require-green-ci.sh with actions:read",
    );
    assert!(
        job_block(&workflow, "meta").contains("needs: ci_gate"),
        "meta, and through it every release job, must need ci_gate",
    );
    for job in ["dispatch-plugin-npm", "dispatch-plugin-hermes"] {
        assert!(
            job_block(&workflow, job).contains("needs: ci_gate"),
            "{job} publishes from a dispatch and must need ci_gate too",
        );
    }
}

/// The workflow token is read-only unless a job asks for more, and the jobs that compile code
/// (the reusable build) never get write access.
#[test]
fn only_the_jobs_that_write_get_write_permissions() {
    let workflow = workflow();
    let top = &workflow[..workflow.find("\njobs:\n").expect("jobs:")];
    assert!(
        top.contains("permissions:\n  contents: read\n") && !top.contains("id-token: write"),
        "the workflow-level token must be contents: read",
    );
    assert!(
        job_block(&workflow, "build").contains("contents: read"),
        "the build job must be read-only"
    );
    let build = read_workflow("release-build.yaml");
    assert!(
        build.contains("permissions:\n  contents: read\n") && !build.contains(": write"),
        "release-build.yaml compiles third-party code and must not hold a write token",
    );
}

/// Every archive is run on a clean runner before it can be uploaded, and the release refuses to go
/// out while plugin content is marked as ahead of its binary.
#[test]
fn archives_are_executed_before_upload() {
    let workflow = workflow();
    let build = read_workflow("release-build.yaml");
    assert!(
        build.contains("scripts/smoke_release_archive.py") && build.contains("needs: [build-native, build-linux]"),
        "release-build.yaml must smoke-test every archive after building it",
    );
    for triple in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ] {
        assert!(
            build.matches(triple).count() >= 2,
            "{triple} must be built and smoke-tested"
        );
    }
    assert!(
        job_block(&workflow, "checksums").contains("needs: [meta, create_release, build]"),
        "the upload job must wait for the smoke-tested build",
    );
    assert!(
        job_block(&workflow, "meta").contains(".github/unreleased-surface.toml"),
        "meta must refuse to release while the unreleased-surface marker exists",
    );
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
        "basemind-x86_64-pc-windows-msvc.zip",
    ] {
        assert!(
            block.contains(asset),
            "finalize_release must require {asset} before promoting"
        );
    }
    assert!(
        block.contains("_checksums.txt"),
        "finalize_release must require the checksums file before promoting",
    );
}

/// macOS Intel is not a release target. The installers, launcher and Homebrew formula refuse it with
/// a clear message, so a workflow that built or required its archive would ship an asset nothing
/// downloads, or block promotion on one that is never built.
#[test]
fn release_has_no_intel_mac_target() {
    let workflow = workflow();
    for needle in ["x86_64-apple-darwin", "macos-26-intel", "ort-dynamic"] {
        assert!(
            !workflow.contains(needle),
            "publish.yaml must not mention `{needle}`: macOS Intel is not built"
        );
    }
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

/// basemind.ai documents what users can install: a push to main builds the site, only a release
/// tag deploys it.
#[test]
fn the_website_deploys_only_from_a_release_tag() {
    let docs = read_workflow("docs.yaml");
    let deploy = job_block(&docs, "deploy");
    assert!(
        deploy.contains("if: startsWith(github.ref, 'refs/tags/v')"),
        "the docs deploy job must run only for a release tag:\n{deploy}"
    );
    assert!(
        deploy.contains("pages: write") && !docs[..docs.find("\njobs:\n").unwrap()].contains("pages: write"),
        "only the deploy job may write to Pages"
    );
}

/// A plugins-only dispatch publishes immutable registry versions, so it runs only from a release
/// tag that names the plugin version, never from a branch.
#[test]
fn plugins_only_dispatch_publishes_only_from_its_release_tag() {
    let workflow = workflow();
    for job in ["dispatch-plugin-npm", "dispatch-plugin-hermes"] {
        let block = job_block(&workflow, job);
        assert!(!block.contains("refs/heads/"), "{job} must not run from a branch");
        assert!(
            block.contains("startsWith(github.ref, 'refs/tags/v')"),
            "{job} must run only from a tag"
        );
        assert!(
            block.contains("\"v$version\" != \"$GITHUB_REF_NAME\""),
            "{job} must refuse a tag that does not name the plugin version"
        );
    }
}
