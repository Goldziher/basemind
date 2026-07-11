//! Regression test for bug #20: comms verbs DO emit JSON when `--json` is passed, so
//! passing `--json` to a comms subcommand must NOT print the
//! "'--json' has no effect on this subcommand" warning.
//!
//! Gated on the same `cfg(all(feature = "comms", unix))` as the `Cmd::Comms` arm.

#![cfg(all(feature = "comms", unix))]

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

/// `--json comms status` may fail to reach a broker (no daemon in CI), but the
/// ignored-flag warning is emitted up front in `warn_ignored_global_flags`, before
/// dispatch — so its presence/absence is independent of the command's success.
#[test]
fn should_not_warn_json_ineffective_on_comms() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(bin())
        .args(["--root", dir.path().to_str().unwrap(), "--json", "comms", "status"])
        .env_remove("RUST_LOG")
        .output()
        .expect("run basemind comms status");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("--json has no effect"),
        "comms consumes `--json`; no ignored-flag warning expected. stderr:\n{stderr}"
    );
}
