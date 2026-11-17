//! Opt-in skips. A test that can check nothing without a resource (a tree-sitter grammar the
//! language pack downloads, an embedding model) fails when the resource is missing, unless the run
//! says it may be: CI captures the output of passing tests, so a printed skip there reads as a pass.

// Each test binary includes this file and uses only the variables it needs.
#![allow(dead_code)]

use std::io::Write;

/// Allows a test to skip when a tree-sitter grammar cannot be loaded (an offline machine).
pub const NO_GRAMMAR: &str = "BASEMIND_TEST_ALLOW_NO_GRAMMAR";

/// Allows a test to skip when no embedding model can be loaded (an offline machine).
pub const NO_EMBEDDER: &str = "BASEMIND_TEST_ALLOW_NO_EMBEDDER";

/// Fails with `what` unless `var` is set; when it is, reports the skip on the stderr stream itself,
/// which libtest does not capture.
pub fn skip_unless_allowed(var: &str, what: &str) {
    assert!(
        std::env::var_os(var).is_some(),
        "{what}. Nothing was checked. Set {var}=1 to allow this skip where the resource cannot be had."
    );
    let _ = writeln!(std::io::stderr().lock(), "SKIP ({var}): {what}");
}
