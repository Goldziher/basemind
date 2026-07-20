//! The ONE-SHOT CLI's git-history reads must come from the index, in both deployments.
//!
//! `basemind serve` was routed through the daemon's git-history index; the CLI (`basemind git …`,
//! which runs the identical MCP tool bodies in-process and exits) was not. Fjall's directory lock is
//! exclusive even for a read-only open, so when a machine daemon is up it is the SOLE holder of
//! `git-history.fjall/` — and a CLI that tries to open it locally cannot: it burns the lock-retry
//! ladder, gives up, and silently live-walks every git query, forever, on the exact machine where the
//! index is built and healthy.
//!
//! These tests pin the routing contract:
//!
//! * **daemon up** (a `comms` build): the CLI reads the daemon's index over the socket, and pays no
//!   lock-retry penalty at startup.
//! * **no daemon** (standalone / non-`comms` build): the CLI opens the index locally, as before.
//!
//! The observable is `search_git_history`'s `partial` flag: it is set exactly when the tool fell back
//! to walking git live (a bounded window, no posting lists), and omitted — `false` — when the answer
//! came from the index. It is both the honest degradation signal the tools already ship and the only
//! thing that separates the two data sources on a repo small enough for the live window to cover.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// A token carried by exactly one commit (in its message body), so the query has a single, stable
/// expected hit whichever data source answered it. Which source that was is read off `partial`.
const BODY_ONLY_TOKEN: &str = "zzqqxclionlytoken";

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// A three-commit repo whose middle commit carries [`BODY_ONLY_TOKEN`] in its message BODY.
fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("write a.rs");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);

    std::fs::write(root.join("b.rs"), b"pub fn beta() {}\n").expect("write b.rs");
    git(root, &["add", "."]);
    git(
        root,
        &[
            "commit",
            "-qm",
            "add beta",
            "-m",
            &format!("body line {BODY_ONLY_TOKEN} here"),
        ],
    );

    std::fs::write(root.join("a.rs"), b"pub fn alpha() -> u32 { 1 }\n").expect("rewrite a.rs");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "widen alpha"]);
    dir
}

/// Run the real one-shot CLI (`basemind --root <root> --json git search <token>`) and parse its JSON.
/// `envs` overlays environment variables on the child only — the isolated cache/comms dirs this
/// process was pinned to are otherwise inherited.
fn cli_search(root: &Path, envs: &[(&str, &str)]) -> Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_basemind"));
    cmd.arg("--root")
        .arg(root)
        .arg("--json")
        .arg("git")
        .arg("search")
        .arg(BODY_ONLY_TOKEN);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("run basemind git search");
    assert!(
        out.status.success(),
        "basemind git search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("CLI did not emit JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

/// Whether the response was live-walked. `partial` is skipped when false, so an absent field means
/// "index-backed" — the same reading the MCP smoke tests use.
fn is_partial(response: &Value) -> bool {
    response.get("partial").and_then(Value::as_bool).unwrap_or(false)
}

/// The one token-carrying commit, served from the index. Panics with the whole response otherwise.
fn assert_index_backed(response: &Value, context: &str) {
    assert!(
        !is_partial(response),
        "{context}: `partial: true` means the CLI live-walked instead of reading the index: {response}"
    );
    let commits = response
        .get("commits")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{context}: commits array: {response}"));
    assert_eq!(
        commits.len(),
        1,
        "{context}: exactly the token-carrying commit matches: {response}"
    );
    assert_eq!(
        commits[0].get("summary").and_then(Value::as_str),
        Some("add beta"),
        "{context}: the hit is the commit whose BODY carries the token: {response}"
    );
}

#[cfg(all(feature = "comms", any(unix, windows)))]
mod with_daemon {
    use std::time::Duration;

    use basemind::comms::client::CommsClient;
    use basemind::comms::ids::AgentId;
    use basemind::comms::singleton::{CommsPaths, comms_socket_path, probe_alive};
    use basemind::git_history::GitHistoryIndex;
    use basemind::git_history::proto::{GitHistoryOp, GitHistoryReply};

    use super::*;

    /// The local-open retry budget the buggy CLI paid on EVERY invocation while a daemon held the
    /// lock: `GH_OPEN_RETRIES` (5) × `GH_OPEN_BACKOFF` (50 ms) of pure sleep, plus six failed fjall
    /// opens. The startup assertion allows half of it as headroom over the no-index baseline, so it
    /// goes red on the retry storm without hard-coding a wall-clock threshold (machine-dependent).
    const LOCK_RETRY_BUDGET_US: u64 = 125_000;

    /// `startup_us` for the CLI's own boot (clap, store open, git-history routing) — everything
    /// before the tool body runs. Best of `RUNS` samples, so a scheduling hiccup cannot inflate it.
    fn best_startup_us(root: &Path, envs: &[(&str, &str)]) -> u64 {
        const RUNS: usize = 3;
        (0..RUNS)
            .map(|_| {
                cli_search(root, envs)
                    .get("startup_us")
                    .and_then(Value::as_u64)
                    .expect("startup_us in the CLI's JSON")
            })
            .min()
            .expect("at least one run")
    }

    fn comms_paths() -> CommsPaths {
        let comms_dir = std::path::PathBuf::from(std::env::var("BASEMIND_COMMS_DIR").expect("isolated comms dir"));
        CommsPaths {
            socket_path: comms_socket_path(&comms_dir),
            comms_dir,
        }
    }

    /// Bring up a REAL `basemind comms daemon` on this process's isolated socket. Never use
    /// `CommsClient::ensure_and_connect` from a test: it execs `current_exe()`, which here is the
    /// libtest harness.
    #[allow(clippy::zombie_processes)]
    fn ensure_real_daemon() {
        let paths = comms_paths();
        if probe_alive(&paths.socket_path) {
            return;
        }
        let _child = Command::new(env!("CARGO_BIN_EXE_basemind"))
            .args(["comms", "daemon"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn comms daemon");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if probe_alive(&paths.socket_path) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("comms daemon did not become ready");
    }

    /// Have the daemon build `root`'s history index — which leaves the daemon HOLDING fjall's
    /// exclusive lock on it (it caches the open database until its idle sweep). That is the state
    /// every CLI invocation on a daemon machine actually runs in.
    async fn daemon_builds_index(root: &Path) {
        ensure_real_daemon();
        let mut client = CommsClient::connect(
            &comms_paths(),
            AgentId::parse("gh-cli-probe").expect("agent id"),
            None,
            Some(root.to_path_buf()),
        )
        .await
        .expect("connect to the daemon");
        match client
            .git_history(root.to_path_buf(), GitHistoryOp::Sync)
            .await
            .expect("daemon git-history sync")
        {
            GitHistoryReply::Synced(_) => {}
            other => panic!("expected Synced, got {other:?}"),
        }
    }

    /// The daemon really is the lock holder: a local open of the same database must fail. Without
    /// this the tests below could pass by accident (a released lock would let the CLI open it
    /// locally), so it is asserted, not assumed.
    fn assert_daemon_holds_the_lock(root: &Path) {
        let dir = basemind::git_history::shared_history_basemind_dir(root);
        let opened = GitHistoryIndex::open(&dir);
        assert!(
            opened.is_err(),
            "precondition: the daemon must hold the exclusive fjall lock on {}",
            dir.display()
        );
    }

    /// THE BUG: with a daemon up (the normal state on a developer machine — `serve` spawns one), the
    /// one-shot CLI could not open the index it cannot lock, so every git query silently live-walked.
    /// It must instead read the daemon's index over the socket, exactly as `serve` does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cli_reads_the_daemons_index_while_the_daemon_holds_the_lock() {
        let dir = build_repo();
        let root = dir.path();

        daemon_builds_index(root).await;
        assert_daemon_holds_the_lock(root);

        let response = cli_search(root, &[]);
        assert_index_backed(&response, "daemon up");
    }

    /// The CLI must not pay the local lock-retry ladder it can never win while a daemon is up: that
    /// was ~1.3 s of dead time on EVERY invocation, including commands that touch no git at all.
    ///
    /// Measured against the same binary with the git-history index switched off (`BASEMIND_GH_INDEX=0`
    /// ⇒ no open attempt at all), so the assertion calibrates itself to the machine instead of
    /// hard-coding a wall-clock threshold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cli_startup_pays_no_lock_retry_penalty_when_a_daemon_is_up() {
        let dir = build_repo();
        let root = dir.path();

        daemon_builds_index(root).await;
        assert_daemon_holds_the_lock(root);

        let baseline_us = best_startup_us(root, &[("BASEMIND_GH_INDEX", "0")]);
        let routed_us = best_startup_us(root, &[]);
        assert!(
            routed_us <= baseline_us + LOCK_RETRY_BUDGET_US,
            "CLI startup with a daemon up ({routed_us} us) must not exceed the no-index baseline \
             ({baseline_us} us) by the fjall lock-retry ladder ({LOCK_RETRY_BUDGET_US} us of headroom); \
             the CLI is retrying a lock the daemon holds instead of proxying to it"
        );
    }

    /// The honest fallback survives: a repo the daemon has never indexed has no index to read, so the
    /// CLI live-walks — and SAYS SO (`partial: true`) rather than pretending the answer is complete.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cli_reports_partial_when_the_daemon_has_no_index_for_the_repo() {
        let dir = build_repo();
        let root = dir.path();
        ensure_real_daemon();

        let response = cli_search(root, &[]);
        assert!(
            is_partial(&response),
            "an unindexed repo must degrade VISIBLY to the live walk: {response}"
        );
    }
}

/// No daemon (a standalone CLI, or any non-`comms` build): the local open is still correct and must
/// keep working. The child gets a private, empty `BASEMIND_COMMS_DIR`, so no daemon is reachable from
/// it whatever the build — the CLI has to open `git-history.fjall/` itself.
#[test]
fn cli_reads_the_local_index_when_no_daemon_is_running() {
    let dir = build_repo();
    let root = dir.path();
    let history_dir = basemind::git_history::shared_history_basemind_dir(root);

    {
        let index = basemind::git_history::GitHistoryIndex::open(&history_dir).expect("open git-history index");
        let repo = basemind::git::Repo::discover(root).expect("discover repo");
        basemind::git_history::builder::sync(&index, &repo, &history_dir).expect("build git-history index");
    }

    let empty_comms = tempfile::tempdir().expect("tempdir");
    let response = cli_search(root, &[("BASEMIND_COMMS_DIR", &empty_comms.path().to_string_lossy())]);
    assert_index_backed(&response, "no daemon");
}
