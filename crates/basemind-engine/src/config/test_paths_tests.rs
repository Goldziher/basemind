//! The default test-path and test-data lists: one source every consumer reads, matching the shapes
//! a large monorepo's integrations tree uses and none of their look-alikes.

use super::{TEST_DATA_GLOBS, TEST_PATH_GLOBS, default_document_exclude};
use crate::config::ConfigV1;
use crate::config::list_only::ListOnly;
use crate::config::roles::{PathRoles, Role};
use crate::config::rules::{TestPaths, compile_rules};

fn owned(globs: &[&str]) -> Vec<String> {
    globs.iter().map(|glob| (*glob).to_string()).collect()
}

/// Paths a large integrations tree holds tests and test data under.
const TEST_PATHS: &[&str] = &[
    "integrations/vendor_a/impl/vendor_a_test_data/get_devices_valid.yaml",
    "integrations/vendor_a/impl/vendor-a-test-data/get_devices_valid.json",
    "integrations/vendor_a/impl/vendor_a_testdata/page_1.json",
    "integrations/vendor_a/impl/test-data/devices.csv",
    "x/y/tests/foo.py",
    "pkg/__fixtures__/a.json",
    "pkg/api_fixtures/response.json",
    "pkg/client_mocks/client.py",
    "svc/test_helpers/builders.py",
    "svc/integration_test/run.go",
    "svc/contract_tests/run.py",
    "ui/view_snapshots/home.txt",
    "render/golden/page.html",
    "render/page.golden",
    "api/client_mock.py",
    "api/device_fixture.json",
    "api/device_fixtures.py",
    "api/client_test.go",
    "api/test_client.py",
    "web/a.test.ts",
    "web/a.spec.ts",
];

/// Production paths that only look like test paths.
const PRODUCTION_PATHS: &[&str] = &[
    "src/contest.rs",
    "latest_data/ok.yaml",
    "attestation/x.md",
    "src/latest/x.py",
    "docs/testing.md",
    "services/fixture_loader.rs",
    "lib/mockingbird.py",
    "pkg/goldensection.go",
];

/// Test data the scan only lists, and test code beside it that stays in the code map.
const LISTED_DATA: &[&str] = &[
    "integrations/vendor_a/impl/vendor_a_test_data/get_devices_valid.yaml",
    "integrations/vendor_a/impl/vendor_a_test_data/helper.py",
    "integrations/vendor_a/impl/vendor_a_testdata/page_1.json",
    "go/pkg/testdata/input.go",
    "pkg/__fixtures__/a.json",
    "pkg/api_fixtures/response.json",
    "x/y/tests/data/devices.yaml",
    "x/y/tests/fixtures/devices.csv",
    "ui/__snapshots__/home.tsx.snap",
    "render/golden/page.html",
    "render/page.golden",
];

const MAPPED_TEST_CODE: &[&str] = &[
    "x/y/tests/foo.py",
    "x/y/tests/fixtures/conftest.py",
    "pkg/__mocks__/client.ts",
    "svc/test_helpers/builders.py",
    "latest_data/ok.yaml",
    "config/devices.yaml",
];

#[test]
fn every_consumer_reads_the_one_test_path_list() {
    let cfg = ConfigV1::with_defaults();
    let tests = owned(TEST_PATH_GLOBS);
    assert_eq!(cfg.scan.test_globs, tests, "[scan] test_globs");
    assert_eq!(cfg.code_search.embed_exclude, tests, "[code_search] embed_exclude");
    let documents = default_document_exclude();
    assert_eq!(cfg.documents.exclude, documents, "[documents] exclude");
    let missing: Vec<&String> = tests.iter().filter(|glob| !documents.contains(glob)).collect();
    assert!(missing.is_empty(), "[documents] exclude lacks {missing:?}");
    let data = owned(TEST_DATA_GLOBS);
    let unlisted: Vec<&String> = data.iter().filter(|glob| !cfg.scan.list_only.contains(glob)).collect();
    assert!(unlisted.is_empty(), "[scan] list_only lacks {unlisted:?}");
}

#[test]
fn test_paths_are_marked_by_every_consumer_and_look_alikes_by_none() {
    let cfg = ConfigV1::with_defaults();
    let marked = TestPaths::from_config(&cfg);
    let roles = PathRoles::new(&cfg.scan).expect("default roles compile");
    let documents = compile_rules(cfg.documents.exclude.iter().map(String::as_str)).expect("compiles");
    let code_embed = compile_rules(cfg.code_search.embed_exclude.iter().map(String::as_str)).expect("compiles");
    for path in TEST_PATHS {
        assert!(marked.is_test(path), "{path} is a test path");
        assert!(roles.has(path, Role::Test), "{path} has the test role");
        assert!(documents.is_match(path), "{path} is not a document");
        assert!(code_embed.is_match(path), "{path} is never embedded");
    }
    for path in PRODUCTION_PATHS {
        assert!(!marked.is_test(path), "{path} is not a test path");
        assert!(!roles.has(path, Role::Test), "{path} has no test role");
        assert!(!code_embed.is_match(path), "{path} may be embedded");
    }
}

#[test]
fn test_data_is_listed_and_test_code_stays_mapped() {
    let list_only = ListOnly::new(&ConfigV1::with_defaults().scan.list_only).expect("defaults compile");
    for path in LISTED_DATA {
        assert!(list_only.is_match(path), "{path} is listed, never opened");
    }
    for path in MAPPED_TEST_CODE.iter().chain(PRODUCTION_PATHS) {
        assert!(!list_only.is_match(path), "{path} stays indexed");
    }
}

#[test]
fn a_negated_entry_re_includes_listed_test_data() {
    let mut patterns = owned(TEST_DATA_GLOBS);
    patterns.push("!integrations/vendor_a/impl/vendor_a_test_data/**".to_string());
    let list_only = ListOnly::new(&patterns).expect("compiles");
    assert!(!list_only.is_match("integrations/vendor_a/impl/vendor_a_test_data/get_devices_valid.yaml"));
    assert!(list_only.is_match("integrations/vendor_b/impl/vendor_b_test_data/get_devices_valid.yaml"));
}
