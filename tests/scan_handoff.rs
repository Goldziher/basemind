//! What a one-shot `basemind scan` leaves for the next process that opens the index.
//!
//! Drives the built binary with its own data home and comms dir, so no daemon of the machine is
//! reached and nothing outside the temp dirs is written.

use std::path::Path;
use std::process::Command;

/// Keyspace directories of every group index under `data_home` that hold at least one table file.
fn keyspaces_with_tables(data_home: &Path) -> usize {
    let groups = data_home.join("cache").join("groups");
    let mut count = 0;
    for group in std::fs::read_dir(&groups).expect("group dirs").flatten() {
        let keyspaces = group.path().join("index.fjall").join("keyspaces");
        let Ok(entries) = std::fs::read_dir(&keyspaces) else {
            continue;
        };
        for keyspace in entries.flatten() {
            let tables = std::fs::read_dir(keyspace.path().join("tables"))
                .map(|dir| dir.flatten().count())
                .unwrap_or(0);
            count += usize::from(tables > 0);
        }
    }
    count
}

/// The scan flushes its rows before it exits. Left in the journal, they would be replayed into
/// memtables by the next writer, the daemon, which then holds them in its heap until each
/// keyspace's memtable fills, and flushes them one keyspace at a time while it serves.
#[test]
fn a_one_shot_scan_leaves_its_rows_in_tables() {
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
    for file in 0..8 {
        let body: String = (0..40)
            .map(|i| format!("pub fn f{file}_{i}(x: u32) -> u32 {{ g{file}_{i}(x) + {i} }}\n"))
            .collect();
        std::fs::write(root.join(format!("m{file}.rs")), body).expect("write");
    }

    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().expect("utf-8 path"), "scan", "--no-git-history"])
        .env("BASEMIND_DATA_HOME", data_home.path())
        .env("BASEMIND_COMMS_DIR", comms_dir.path())
        .output()
        .expect("run basemind scan");
    assert!(
        output.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let flushed = keyspaces_with_tables(data_home.path());
    assert!(
        flushed >= 5,
        "only {flushed} index keyspaces hold a table: the rows were left in the journal"
    );
}
