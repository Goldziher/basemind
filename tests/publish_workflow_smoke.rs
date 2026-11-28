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
        meta.contains("scripts/check_vendored_crates.py --release"),
        "meta must refuse vendored crates that drifted from their published version, and a crate \
         crates.io does not have: trusted publishing cannot create one",
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

/// basemind depends on basemind-agents by path and version (`serve --lead`, ADR-0040): crates.io
/// resolves it by version, so it is published first, and moves with basemind's version.
#[test]
fn basemind_agents_is_published_before_basemind_and_bumped_with_it() {
    let workflow = workflow();
    let publish = job_block(&workflow, "publish_crates");
    let agents = publish
        .find("publish basemind-agents")
        .expect("basemind-agents is published");
    let basemind = publish
        .find("publish basemind --features full")
        .expect("basemind is published");
    assert!(
        agents < basemind,
        "basemind-agents must be on crates.io before basemind"
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let bump = std::fs::read_to_string(root.join("scripts/release-bump.sh")).expect("release-bump.sh");
    assert!(
        bump.contains("crates/basemind-agents/Cargo.toml"),
        "the bump moves the crate's version"
    );
    assert!(bump.contains("basemind-agents = "), "and the root's requirement on it");
    let check = std::fs::read_to_string(root.join("scripts/check_release_versions.py")).expect("checker");
    assert!(
        check.contains("crates/basemind-agents/Cargo.toml"),
        "the release gate checks it"
    );
    let vendored = std::fs::read_to_string(root.join("scripts/check_vendored_crates.py")).expect("drift check");
    assert!(vendored.contains("\"basemind-agents\""), "and its drift from crates.io");
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

/// Every archive is packaged with the generated THIRD_PARTY_LICENSES: one job renders it from the
/// locked graph, and each build job waits for it and downloads it before packaging.
#[test]
fn every_archive_is_packaged_with_the_third_party_licenses() {
    let build = read_workflow("release-build.yaml");
    assert!(
        job_block(&build, "third-party-licenses").contains("run: mise run release:licenses"),
        "release-build.yaml must generate THIRD_PARTY_LICENSES with the release:licenses task",
    );
    for job in ["build-native", "build-linux"] {
        let block = job_block(&build, job);
        assert!(
            block.contains("needs: third-party-licenses"),
            "{job} must wait for THIRD_PARTY_LICENSES"
        );
        let download = block
            .find("name: third-party-licenses")
            .unwrap_or_else(|| panic!("{job} must download THIRD_PARTY_LICENSES"));
        let package = block
            .find("scripts/package-release.sh")
            .unwrap_or_else(|| panic!("{job} must package the release"));
        assert!(
            download < package,
            "{job} must download THIRD_PARTY_LICENSES before packaging"
        );
    }
}

/// Every change and every release checks the hand-written native-code notices against Cargo.lock
/// before rendering THIRD_PARTY_LICENSES, so a stale entry or a licence about.toml does not accept
/// fails on the pull request rather than at release time.
#[test]
fn native_notices_are_checked_before_third_party_licenses_render() {
    let check = "run: python3 scripts/check_native_notices.py";
    let render = "run: mise run release:licenses";
    for file in ["ci.yaml", "release-build.yaml"] {
        let workflow = read_workflow(file);
        let block = job_block(&workflow, "third-party-licenses");
        let checked = block
            .find(check)
            .unwrap_or_else(|| panic!("{file} must check the native-code notices"));
        let rendered = block
            .find(render)
            .unwrap_or_else(|| panic!("{file} must render THIRD_PARTY_LICENSES"));
        assert!(
            checked < rendered,
            "{file} must check the native-code notices before rendering THIRD_PARTY_LICENSES"
        );
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

/// The github-pages environment admits `main` and `v*` tags, and the site documents the released
/// binary, so only a pushed release tag deploys it; main and pull requests build and check links.
#[test]
fn the_website_deploys_only_from_a_release_tag() {
    let docs = read_workflow("docs.yaml");
    let deploy = job_block(&docs, "deploy");
    let triggers = &docs[..docs.find("\njobs:\n").unwrap()];
    assert!(
        deploy.contains("if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"),
        "the docs deploy job must run only for a pushed release tag:\n{deploy}"
    );
    assert!(
        triggers.contains("tags: [\"v*\"]"),
        "a release tag must trigger the docs workflow"
    );
    assert!(
        deploy.contains("pages: write") && !triggers.contains("pages: write"),
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

/// CI decides whether the pinned version is released by its checksums file, as the plugin launcher
/// does. The tag page answers 200 for a bare tag or a draft release, whose assets are not public:
/// reading that as a release failed the skew legs, and with them the publish's ci_gate, for as long
/// as the release that gate held back did not exist.
#[test]
fn the_skew_gate_reads_a_release_from_its_checksums_not_its_tag() {
    let ci = read_workflow("ci.yaml");
    let start = ci
        .find("- name: Fetch release binaries for the upgrade test and the skew gate")
        .expect("the skew gate's fetch step");
    let rest = &ci[start + 1..];
    let step = &rest[..rest.find("\n      - name:").unwrap_or(rest.len())];
    assert!(
        !step.contains("/releases/tag/"),
        "a tag page exists before the release is public:\n{step}"
    );
    assert!(
        step.contains("/releases/download/v$pinned/basemind_${pinned}_checksums.txt"),
        "the pinned release must be detected by its checksums file:\n{step}",
    );
}

/// The marketplaces serve `./plugin` from main, so main reaches the release commit only once the
/// release is public (ADR-0046): earlier, every auto-updating host pinned a binary with no assets.
#[test]
fn main_advances_to_the_release_only_after_it_is_public() {
    let workflow = workflow();
    let block = job_block(&workflow, "advance_main");
    assert_gated_on_finalize(&workflow, "advance_main");
    assert!(
        block.contains("git merge-base --is-ancestor origin/main \"$sha\"")
            && block.contains("git push origin \"$sha:refs/heads/main\"")
            && !block.contains("--force"),
        "advance_main must only fast-forward main:\n{block}"
    );
    assert!(
        block.contains("gh workflow run ci.yaml --ref main") && block.contains("actions: write"),
        "a push with the workflow token starts no ci run, so advance_main must dispatch one:\n{block}"
    );
}

/// The names of every job under `jobs:`, in file order.
fn job_names(workflow: &str) -> Vec<String> {
    let jobs = &workflow[workflow.find("\njobs:\n").expect("jobs:") + "\njobs:\n".len()..];
    jobs.lines()
        .filter_map(|line| {
            let name = line.strip_prefix("  ")?.strip_suffix(':')?;
            let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            valid.then(|| name.to_string())
        })
        .collect()
}

/// The jobs a job lists in `needs:`, as an inline list, a single name or a block list.
fn needs_of(block: &str) -> Vec<String> {
    let mut lines = block.lines();
    let Some(header) = lines.find(|line| line.starts_with("    needs:")) else {
        return Vec::new();
    };
    let inline = header.trim_start_matches("    needs:").trim();
    if !inline.is_empty() {
        return inline
            .trim_matches(|c: char| c == '[' || c == ']')
            .split(',')
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
    }
    lines
        .map_while(|line| line.strip_prefix("      - "))
        .map(|name| name.trim().to_string())
        .collect()
}

/// A job's `if:` expression, including the lines of a folded scalar.
fn condition_of(block: &str) -> Option<String> {
    let mut lines = block.lines().skip_while(|line| !line.starts_with("    if:"));
    let header = lines.next()?;
    let rest = lines.take_while(|line| line.starts_with("      "));
    Some(std::iter::once(header).chain(rest).collect::<Vec<_>>().join("\n"))
}

/// Whether an `if:` can evaluate false: `always()` only overrides the implicit "needs succeeded"
/// check, so a condition that keeps any other term next to it (`always() && inputs.x`) still skips.
fn can_skip(condition: &str) -> bool {
    let rest = condition.replace("always()", "");
    let rest = rest.trim_start().strip_prefix("if:").unwrap_or(&rest).to_owned();
    rest.chars().any(|c| !c.is_whitespace() && !"(){}$&>-".contains(c))
}

/// The jobs that can be skipped on a run: one whose `if:` can evaluate false, or one with no `if:`
/// that needs such a job (a skipped need skips its dependants).
fn skippable_jobs(workflow: &str) -> Vec<String> {
    let jobs: Vec<(String, Vec<String>, Option<String>)> = job_names(workflow)
        .into_iter()
        .map(|name| {
            let block = job_block(workflow, &name);
            let (needs, condition) = (needs_of(block), condition_of(block));
            (name, needs, condition)
        })
        .collect();
    let mut skippable: Vec<String> = jobs
        .iter()
        .filter(|(_, _, condition)| condition.as_deref().is_some_and(can_skip))
        .map(|(name, ..)| name.clone())
        .collect();
    loop {
        let before = skippable.len();
        for (name, needs, condition) in &jobs {
            if condition.is_none() && !skippable.contains(name) && needs.iter().any(|need| skippable.contains(need)) {
                skippable.push(name.clone());
            }
        }
        if skippable.len() == before {
            return skippable;
        }
    }
}

/// The jobs that need a skippable job but do not say `always()`: on a rerun the skippable one is
/// skipped, and without `always()` so are they. The release chain is the one intended exception.
fn jobs_missing_always(workflow: &str, allowed: &[&str]) -> Vec<String> {
    let skippable = skippable_jobs(workflow);
    job_names(workflow)
        .into_iter()
        .filter(|name| !allowed.contains(&name.as_str()))
        .filter(|name| {
            let block = job_block(workflow, name);
            needs_of(block).iter().any(|need| skippable.contains(need))
                && !condition_of(block).is_some_and(|c| c.contains("always()"))
        })
        .collect()
}

/// A rerun for a tag whose release is already complete skips `create_release`, `build` and
/// `checksums`; every job fed by one of them (or by `meta`, skipped on a plugins-only dispatch)
/// must still run, so it sets `always()`. Without it the 0.34.0 rerun skipped every plugin job.
#[test]
fn every_job_fed_by_a_skippable_ancestor_sets_always() {
    const RELEASE_CHAIN: [&str; 3] = ["create_release", "build", "checksums"];
    let workflow = workflow();
    assert_eq!(
        jobs_missing_always(&workflow, &RELEASE_CHAIN),
        Vec::<String>::new(),
        "these jobs need a job that a rerun skips, so they need `always()` in their `if:`"
    );
    let skippable = skippable_jobs(&workflow);
    for job in RELEASE_CHAIN {
        assert!(
            skippable.iter().any(|name| name == job),
            "{job} is expected to be skippable on a rerun"
        );
        assert!(
            job_block(&workflow, job).contains("release_assets_exist") || job == "checksums",
            "{job} must skip on a complete release, not on a failed need"
        );
    }
    for job in [
        "finalize_release",
        "publish_plugin_npm",
        "publish_plugin_artifacts",
        "publish_homebrew",
    ] {
        assert!(
            condition_of(job_block(&workflow, job)).is_some_and(|c| c.contains("always()")),
            "{job} must run on a rerun"
        );
    }
}

/// The check itself: a job that needs a skippable one without `always()` is reported, one with it is not.
#[test]
fn the_always_check_reports_a_job_that_a_skipped_need_would_skip() {
    let workflow = "name: t\njobs:\n  a:\n    if: ${{ inputs.x }}\n    runs-on: x\n  b:\n    needs: [a]\n    if: >-\n      ${{ always() }}\n    runs-on: x\n  c:\n    needs: [a]\n    if: ${{ inputs.y }}\n    runs-on: x\n  d:\n    needs:\n      - c\n    runs-on: x\n";
    assert_eq!(jobs_missing_always(workflow, &[]), ["c", "d"]);
    assert_eq!(jobs_missing_always(workflow, &["c"]), ["d"]);
}

/// `always()` next to another term does not make the job unskippable: `a` is skipped when `inputs.x` is
/// false, so the unconditional `b` that needs it is reported.
#[test]
fn the_always_check_reports_a_job_needing_one_guarded_by_always_and_another_term() {
    let workflow = "name: t\njobs:\n  a:\n    if: ${{ always() && inputs.x }}\n    runs-on: x\n  b:\n    needs: [a]\n    runs-on: x\n";
    assert_eq!(jobs_missing_always(workflow, &[]), ["b"]);
}

#[test]
fn a_bare_always_condition_is_not_skippable() {
    for condition in [
        "    if: ${{ always() }}",
        "    if: >-\n      ${{ always() }}",
        "    if: always()",
    ] {
        assert!(!can_skip(condition), "{condition}");
    }
    assert!(can_skip("    if: >-\n      ${{ !inputs.plugins-only && (always()) }}"));
}

/// The release notes are cut under GitHub's 125000-character body limit (HTTP 422 above it); the
/// CHANGELOG sections themselves are run through the step by `scripts/tests/test_release_notes_cap.py`.
#[test]
fn the_release_notes_are_capped_below_githubs_body_limit() {
    let workflow = workflow();
    let step = job_block(&workflow, "create_release");
    let limit: u32 = step
        .lines()
        .find_map(|line| line.trim().strip_prefix("limit = "))
        .expect("the cap step sets `limit = N`")
        .parse()
        .expect("the limit is a number");
    assert!(
        limit <= 125_000,
        "a release body over 125000 characters is refused: {limit}"
    );
    let extract = step
        .find("changelog_section.py")
        .expect("notes come from the CHANGELOG");
    let cap = step.find("limit = ").expect("the cap step");
    let create = step.find("gh release create").expect("the create step");
    assert!(
        extract < cap && cap < create,
        "the notes are extracted, then capped, then used to create the release"
    );
}

/// The CI gate ignores the failed jobs of an earlier attempt by the names this workflow gives its
/// jobs, so every job needs a `name:` at job level (a job without one is named after its key and
/// would count against the rerun), and the names must be unique.
#[test]
fn every_job_has_a_name_the_ci_gate_can_ignore() {
    let workflow = workflow();
    let mut names = Vec::new();
    for job in job_names(&workflow) {
        let name = job_block(&workflow, &job)
            .lines()
            .find_map(|line| line.strip_prefix("    name: "))
            .unwrap_or_else(|| panic!("job `{job}` needs a `name:` the CI gate can match"));
        names.push(name.to_string());
    }
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "job names must be unique: {names:?}");
    let gate =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/require-green-ci.sh"))
            .expect("require-green-ci.sh");
    assert!(
        gate.contains("workflows/publish.yaml") && gate.contains("startswith($o + \" / \")"),
        "the gate must derive the ignored names from publish.yaml, and the reusable build's jobs by prefix"
    );
}

/// The value of a step input (`key: "value"`) in a workflow, if the workflow sets it.
fn step_input(workflow: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    workflow
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().trim_matches('"').to_string())
}

/// Whether the plugin config generates a runtime (`runtimes = [..]` in the `[plugin]` table).
fn plugin_generates_runtime(runtime: &str) -> bool {
    let config =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugin/.ai-rulez/config.plugins.toml"))
            .expect("read plugin/.ai-rulez/config.plugins.toml");
    let runtimes = config
        .lines()
        .find_map(|line| line.strip_prefix("runtimes = ["))
        .expect("the plugin config lists its runtimes");
    runtimes
        .split(',')
        .any(|entry| entry.trim().trim_end_matches(']').trim().trim_matches('"') == runtime)
}

/// The packaging step emits a Cursor team-marketplace index only for a plugin that has a generated
/// `.cursor-plugin/plugin.json`. v0.34.0 asked for the index while the config omitted the runtime,
/// and the publish failed on "plugin has no .cursor-plugin/plugin.json". Both workflows must ask for
/// the emitter exactly when the config generates the runtime, and the generated manifest must exist.
#[test]
fn the_cursor_marketplace_emitter_matches_the_generated_runtimes() {
    let generates_cursor = plugin_generates_runtime("cursor");
    for name in ["publish.yaml", "plugin-artifacts.yaml"] {
        let flag = step_input(&read_workflow(name), "cursor-marketplace");
        assert_eq!(
            flag.as_deref() == Some("true"),
            generates_cursor,
            "{name} sets cursor-marketplace to {flag:?} but the plugin config {} the cursor runtime",
            if generates_cursor { "generates" } else { "omits" },
        );
    }
    if generates_cursor {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugin/.cursor-plugin/plugin.json");
        assert!(
            manifest.is_file(),
            "the config generates cursor but {} is missing; run `mise run plugin:generate`",
            manifest.display()
        );
    }
}
