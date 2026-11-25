//! A one-shot `basemind scan` of the working tree returns without embedding and reports what it
//! left for the background pass (#454).
//!
//! Drives the built binary with its own data home and comms dir, so no daemon of the machine is
//! reached and nothing outside the temp dirs is written. `BASEMIND_EMBED_BACKGROUND=false` keeps the
//! scan from starting the worker, so no model is downloaded and no process outlives the test.

#![cfg(feature = "documents")]

use std::process::Command;

#[test]
fn a_one_shot_scan_reports_its_documents_pending_instead_of_embedding_them() {
    let data_home = tempfile::tempdir().expect("data home");
    let comms_dir = tempfile::tempdir().expect("comms dir");
    let repo = tempfile::tempdir().expect("repo");
    let root = repo.path();
    let status = basemind::testing::git_command()
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .expect("git in PATH");
    assert!(status.success());
    std::fs::write(
        root.join("guide.md"),
        "# Guide\n\nHow the harbour is run, long enough to index as a document.\n",
    )
    .expect("guide.md");

    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args([
            "--root",
            root.to_str().expect("utf-8 path"),
            "--json",
            "scan",
            "--no-git-history",
        ])
        .env("BASEMIND_DATA_HOME", data_home.path())
        .env("BASEMIND_COMMS_DIR", comms_dir.path())
        .env("BASEMIND_NO_AUTOSPAWN", "1")
        .env("BASEMIND_EMBED_BACKGROUND", "false")
        .output()
        .expect("run basemind scan");
    assert!(
        output.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).expect("the scan answers JSON");
    assert_eq!(answer["docs_indexed"], 1, "{answer}");
    assert_eq!(
        answer["embeddings"],
        serde_json::json!({
            "pending_documents": 1,
            "pending_code": false,
            "pending_policy": false,
            "background": "disabled",
        }),
        "{answer}"
    );
}
