//! Buckets and the order of work of `code impact` (#209).

use super::*;

fn hit(path: &str, depth: u8) -> ImpactHit {
    ImpactHit {
        path: RelPath::from(path),
        depth,
        via: RelPath::from("target.rs"),
        in_cycle: false,
    }
}

fn caller(path: &str, depth: u8) -> ImpactCaller {
    ImpactCaller {
        path: RelPath::from(path),
        line: 1,
        depth,
        symbol: None,
        calls: "f".to_string(),
        resolution: "imported",
        proven: true,
    }
}

#[test]
fn a_path_is_bucketed_as_external_test_config_or_prod() {
    use ImpactBucket::{Config, External, Prod, Test};
    let cases = [
        ("src/engine/store.rs", Prod),
        ("src/testing_utils.rs", Prod),
        ("src/latest.rs", Prod),
        ("src/Contest.java", Prod),
        ("src/api/handler.ts", Prod),
        ("tests/layering.rs", Test),
        ("src/engine/ops/impact_tests.rs", Test),
        ("pkg/store_test.go", Test),
        ("pkg/test_store.py", Test),
        ("pkg/conftest.py", Test),
        ("web/button.test.tsx", Test),
        ("web/button.spec.ts", Test),
        ("web/__tests__/button.ts", Test),
        ("app/src/test/java/StoreTest.java", Test),
        ("spec/store_spec.rb", Test),
        ("testdata/sample.go", Test),
        ("Cargo.toml", Config),
        ("deploy/app.yaml", Config),
        (".github/scripts/release.py", Config),
        ("build.rs", Config),
        ("webpack.config.js", Config),
        ("config/settings.py", Config),
        ("Dockerfile", Config),
        ("vendor/lib/lib.go", External),
        ("web/node_modules/pkg/index.js", External),
        ("third_party/zlib/zlib.c", External),
        ("vendor/lib/lib_test.go", External),
        ("/abs/outside/root/lib.rs", External),
    ];
    for (path, expected) in cases {
        assert_eq!(bucket_of(&RelPath::from(path)), expected, "{path}");
    }
}

#[test]
fn the_order_puts_targets_first_then_nearest_files_prod_config_test_external() {
    let targets = [RelPath::from("src/core.rs")];
    let affected = [
        hit("tests/core_it.rs", 1),
        hit("vendor/x/x.go", 1),
        hit("src/b.rs", 1),
        hit("Cargo.toml", 1),
        hit("src/a.rs", 1),
        hit("src/deep.rs", 2),
    ];
    let (impacted, buckets, order) = order_of_work(&targets, &affected, &[]);
    let rows: Vec<(&str, u8, ImpactBucket)> = order
        .iter()
        .map(|step| (step.path.as_str().unwrap(), step.depth, step.bucket))
        .collect();
    assert_eq!(
        rows,
        [
            ("src/core.rs", 0, ImpactBucket::Prod),
            ("src/a.rs", 1, ImpactBucket::Prod),
            ("src/b.rs", 1, ImpactBucket::Prod),
            ("Cargo.toml", 1, ImpactBucket::Config),
            ("tests/core_it.rs", 1, ImpactBucket::Test),
            ("vendor/x/x.go", 1, ImpactBucket::External),
            ("src/deep.rs", 2, ImpactBucket::Prod),
        ]
    );
    assert_eq!(impacted, 6);
    assert_eq!(
        buckets,
        ImpactBuckets {
            prod: 3,
            config: 1,
            test: 1,
            external: 1
        }
    );
}

#[test]
fn a_file_reached_twice_counts_once_at_its_nearest_depth_and_a_target_is_not_impacted() {
    let targets = [RelPath::from("src/core.rs")];
    let affected = [hit("src/a.rs", 2), hit("src/core.rs", 1)];
    let callers = [
        caller("src/a.rs", 1),
        caller("tests/a_tests.rs", 2),
        caller("src/core.rs", 1),
    ];
    let (impacted, buckets, order) = order_of_work(&targets, &affected, &callers);
    let rows: Vec<(&str, u8)> = order
        .iter()
        .map(|step| (step.path.as_str().unwrap(), step.depth))
        .collect();
    assert_eq!(rows, [("src/core.rs", 0), ("src/a.rs", 1), ("tests/a_tests.rs", 2)]);
    assert_eq!(impacted, 2);
    assert_eq!((buckets.prod, buckets.test), (1, 1));
}

#[test]
fn nothing_impacted_leaves_only_the_targets() {
    let (impacted, buckets, order) = order_of_work(&[RelPath::from("src/core.rs")], &[], &[]);
    assert_eq!((impacted, buckets), (0, ImpactBuckets::default()));
    assert_eq!(order.len(), 1);
}
