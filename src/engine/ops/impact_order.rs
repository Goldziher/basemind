//! The impact answer's second half (#209): every impacted file sorted into a bucket (production
//! code, configuration, tests, external code) and the order to work through them.

use std::collections::BTreeMap;

use crate::engine::types::graph::{ImpactBucket, ImpactBuckets, ImpactCaller, ImpactHit, ImpactResponse, ImpactStep};
use crate::path::RelPath;

/// Directories holding code that is not the repository's own.
const EXTERNAL_DIRS: &[&str] = &[
    "vendor",
    "vendored",
    "node_modules",
    "third_party",
    "thirdparty",
    "site-packages",
    ".venv",
];

/// Directories holding tests and their support files.
const TEST_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "spec",
    "specs",
    "e2e",
    "testdata",
    "test-data",
    "fixtures",
    "__fixtures__",
    "__mocks__",
];

/// Directories holding configuration and automation.
const CONFIG_DIRS: &[&str] = &[".github", ".circleci", ".config", "config", "configs", "ci"];

/// Extensions of configuration files.
const CONFIG_EXTENSIONS: &[&str] = &[
    "toml",
    "yaml",
    "yml",
    "json",
    "jsonc",
    "json5",
    "ini",
    "cfg",
    "conf",
    "properties",
    "env",
    "xml",
    "lock",
    "tf",
    "tfvars",
];

/// File names that configure a build or a tool.
const CONFIG_NAMES: &[&str] = &[
    "build.rs",
    "setup.py",
    "dockerfile",
    "makefile",
    "justfile",
    "jenkinsfile",
    "build.gradle",
    "build.gradle.kts",
    "rakefile",
    "gemfile",
];

/// Name suffixes (before the extension) of test files, in lower case.
const TEST_STEM_SUFFIXES: &[&str] = &["_test", "_tests", ".test", ".spec", "_spec"];

/// Whether a file name marks a test: `test_x`, `x_test`, `x.spec`, `conftest`, or a `FooTest` class.
fn is_test_name(raw_name: &str) -> bool {
    let raw_stem = raw_name.rsplit_once('.').map_or(raw_name, |(stem, _)| stem);
    let stem = raw_stem.to_lowercase();
    stem == "conftest"
        || stem.starts_with("test_")
        || TEST_STEM_SUFFIXES.iter().any(|suffix| stem.ends_with(suffix))
        || (raw_stem.len() > "Test".len() && (raw_stem.ends_with("Test") || raw_stem.ends_with("Tests")))
}

/// The bucket of one repository path: external code first, then tests, then configuration, else
/// production code. Judged from the path alone.
pub(super) fn bucket_of(path: &RelPath) -> ImpactBucket {
    let raw = path.to_str_lossy();
    let text = raw.to_lowercase();
    let (dirs, name) = text.rsplit_once('/').unwrap_or(("", text.as_str()));
    let raw_name = raw.rsplit_once('/').map_or(raw.as_ref(), |(_, name)| name);
    let in_dir = |names: &[&str]| dirs.split('/').any(|dir| names.contains(&dir));
    if path.is_external() || in_dir(EXTERNAL_DIRS) {
        return ImpactBucket::External;
    }
    if in_dir(TEST_DIRS) || is_test_name(raw_name) {
        return ImpactBucket::Test;
    }
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
    let config_stem = stem == "config" || stem.ends_with(".config");
    if in_dir(CONFIG_DIRS) || CONFIG_NAMES.contains(&name) || CONFIG_EXTENSIONS.contains(&extension) || config_stem {
        return ImpactBucket::Config;
    }
    ImpactBucket::Prod
}

/// The impacted files, bucketed and ordered, from the importers reached and the call sites found.
///
/// A target comes first at depth 0; an impacted file follows at the fewest hops it was reached in
/// (import or call), nearest first, production before configuration before tests before external
/// code within a depth, then by path. Targets are not counted as impacted.
pub(super) fn order_of_work(
    targets: &[RelPath],
    affected: &[ImpactHit],
    callers: &[ImpactCaller],
) -> (usize, ImpactBuckets, Vec<ImpactStep>) {
    let mut depths: BTreeMap<&RelPath, u8> = BTreeMap::new();
    let reached = affected
        .iter()
        .map(|hit| (&hit.path, hit.depth))
        .chain(callers.iter().map(|caller| (&caller.path, caller.depth)));
    for (path, depth) in reached {
        depths
            .entry(path)
            .and_modify(|seen| *seen = (*seen).min(depth))
            .or_insert(depth);
    }
    for target in targets {
        depths.remove(target);
    }
    let mut buckets = ImpactBuckets::default();
    let mut order: Vec<ImpactStep> = depths
        .into_iter()
        .map(|(path, depth)| {
            let bucket = bucket_of(path);
            buckets.add(bucket);
            ImpactStep {
                path: path.clone(),
                depth,
                bucket,
            }
        })
        .collect();
    order.sort_by(|a, b| (a.depth, a.bucket, &a.path).cmp(&(b.depth, b.bucket, &b.path)));
    let impacted = order.len();
    let mut steps: Vec<ImpactStep> = targets
        .iter()
        .map(|path| ImpactStep {
            path: path.clone(),
            depth: 0,
            bucket: bucket_of(path),
        })
        .collect();
    steps.extend(order);
    (impacted, buckets, steps)
}

/// Fill the response's `impacted_files`, `buckets` and `order` from its walks.
pub(super) fn finish(response: &mut ImpactResponse) {
    let (impacted, buckets, order) = order_of_work(&response.targets, &response.affected, &response.callers);
    response.impacted_files = impacted;
    response.buckets = buckets;
    response.order = order;
}

#[cfg(test)]
#[path = "impact_order_tests.rs"]
mod tests;
