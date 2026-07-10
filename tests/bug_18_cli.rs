//! Regression test for bug #18 (serve half): `basemind serve --view <named>` for a view
//! that was never scanned must error with actionable guidance instead of silently opening
//! an empty, working-like index. The working view stays exempt (it auto-scans on first run).
//!
//! Unix-only for the harness conventions; the guard itself is platform-independent.

#![cfg(unix)]

use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

#[test]
fn serve_errors_on_never_scanned_named_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn a() {}\n").unwrap();

    let output = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "serve", "--view", "rev-deadbee"])
        .stdin(Stdio::null())
        .output()
        .expect("run basemind serve");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(0),
        "serve on a never-scanned named view must fail, not silently serve empty; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("has not been scanned") && stderr.contains("rev-deadbee"),
        "error must name the unscanned view and how to fix it; got:\n{stderr}"
    );
}
