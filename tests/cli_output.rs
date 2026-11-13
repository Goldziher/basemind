//! End-to-end checks of the CLI's output contract: `--format toon` renders, human output never
//! shortens a body, the timing footer stays off stdout, and `--cursor` / `--max-tokens` reach the
//! MCP tool.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

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

/// A scanned repo with three files; `a.rs` holds one function whose body is far past 200 chars.
fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    let long = "z".repeat(400);
    std::fs::write(
        root.join("a.rs"),
        format!("pub fn alpha() {{\n    let s = \"{long}\";\n    let _ = s;\n}}\n"),
    )
    .unwrap();
    std::fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();
    std::fs::write(root.join("c.rs"), b"pub fn gamma() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let scan = Command::new(bin())
        .args(["--root", root.to_str().unwrap(), "scan"])
        .output()
        .expect("scan");
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );
    dir
}

fn run(root: &Path, args: &[&str]) -> Output {
    let out = Command::new(bin())
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .expect("run basemind");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn code_expand_prints_the_whole_body_and_keeps_timing_off_stdout() {
    let dir = build_repo();
    let out = run(dir.path(), &["code", "expand", "a.rs", "alpha"]);
    let text = stdout(&out);
    assert!(text.contains(&"z".repeat(400)), "the body was shortened:\n{text}");
    assert!(!text.contains('…'), "no ellipsis in a raw body:\n{text}");
    assert!(!text.contains("startup"), "timing footer leaked onto stdout:\n{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("startup"), "timing footer must be on stderr:\n{err}");
}

#[test]
fn format_toon_renders_and_json_wins_over_it() {
    let dir = build_repo();
    let toon = stdout(&run(dir.path(), &["code", "symbols", "a", "--format", "toon"]));
    assert!(
        toon.contains("results[") && toon.contains("]{"),
        "expected a TOON table:\n{toon}"
    );
    let json = stdout(&run(
        dir.path(),
        &["--json", "code", "symbols", "a", "--format", "toon"],
    ));
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("--json prints JSON even with toon");
    assert!(parsed["results"].is_array(), "{json}");
}

#[test]
fn cursor_resumes_a_paged_listing() {
    let dir = build_repo();
    let first = stdout(&run(dir.path(), &["--json", "code", "files", "--limit", "1"]));
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a 3-file listing at limit 1 has a next page");
    let second = stdout(&run(
        dir.path(),
        &["--json", "code", "files", "--limit", "1", "--cursor", cursor],
    ));
    let second: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_ne!(
        first["files"][0], second["files"][0],
        "the cursor must advance past the first page"
    );
}

#[test]
fn max_tokens_reaches_the_tool_and_flags_the_budget() {
    let dir = build_repo();
    let out = stdout(&run(
        dir.path(),
        &["--json", "code", "symbols", "a", "--max-tokens", "1"],
    ));
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["budgeted"], true,
        "a 1-token budget must trim and flag the list:\n{out}"
    );
}

#[test]
fn explicit_shared_base_verification_uses_standalone_for_dirty_worktrees() {
    basemind::store::init_isolated_cache();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("example.rs"), "pub fn example() { called(); }\n").unwrap();
    std::fs::write(
        root.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n[documents]\nenabled = false\nembed = false\n[code_search]\nenabled = false\nembed = false\n",
    )
    .unwrap();
    for index in 0..10 {
        std::fs::write(root.join(format!("keep_{index}.rs")), format!("fn keep_{index}() {{}}")).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
    let verify = || {
        Command::new(bin())
            .args([
                "--root",
                root.to_str().unwrap(),
                "scan",
                "--verify-shared-base",
                "--no-git-history",
            ])
            .output()
            .unwrap()
    };
    let clean = verify();
    let message = String::from_utf8_lossy(&clean.stderr);
    assert!(clean.status.success(), "{message}");
    assert!(message.contains("Shared base matches"), "{message}");
    std::fs::write(root.join("example.rs"), "pub fn changed() { called(); }\n").unwrap();
    let dirty = verify();
    let message = String::from_utf8_lossy(&dirty.stderr);
    assert!(dirty.status.success(), "{message}");
    assert!(message.contains("Using the standalone index"), "{message}");
    if !cfg!(any(feature = "code-intel-js", feature = "code-intel-stack")) {
        let opted_in = || {
            Command::new(bin())
                .env("BASEMIND_INDEX_LAYERED", "1")
                .args(["--root", root.to_str().unwrap(), "scan", "--no-git-history"])
                .output()
                .unwrap()
        };
        let first = opted_in();
        assert!(first.status.success(), "{:?}", first.stderr);
        let second = opted_in();
        let message = String::from_utf8_lossy(&second.stderr);
        assert!(second.status.success(), "{message}");
        assert!(message.contains("Shared worktree index ready"), "{message}");
    }
}

#[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
#[test]
fn cold_dirty_scan_publishes_delta_without_standalone_fjall() {
    basemind::store::init_isolated_cache();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("sample.rs"), "fn sample() { before(); }").unwrap();
    std::fs::write(
        root.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n[documents]\nenabled=false\nembed=false\n[code_search]\nenabled=false\nembed=false\n",
    )
    .unwrap();
    for index in 0..10 {
        std::fs::write(root.join(format!("keep_{index}.rs")), format!("fn keep_{index}() {{}}")).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
    let mut config = basemind::config::Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    assert!(
        basemind::store_base::open_clean_worktree(root, &config)
            .unwrap()
            .is_some()
    );
    std::fs::write(root.join("sample.rs"), "fn after_symbol() { after(); }").unwrap();
    let refused = Command::new(bin())
        .env("BASEMIND_INDEX_LAYERED", "1")
        .env("BASEMIND_INDEX_MAX_GROUP_BYTES", "1")
        .args(["--root", root.to_str().unwrap(), "scan", "--no-git-history"])
        .output()
        .unwrap();
    assert!(refused.status.success(), "{}", String::from_utf8_lossy(&refused.stderr));
    assert!(!String::from_utf8_lossy(&refused.stderr).contains("Shared worktree index ready"));
    assert!(!basemind::store_base::has_published_dirty_worktree(root, &config).unwrap());
    let working = basemind::store::workspace_cache_dir(root).join("views/working");
    std::fs::remove_dir_all(working).unwrap();
    let result = Command::new(bin())
        .env("BASEMIND_INDEX_LAYERED", "1")
        .args(["--root", root.to_str().unwrap(), "scan", "--no-git-history"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert!(stderr.contains("Shared worktree index ready"), "{stderr}");
    let cache = basemind::store::workspace_cache_dir(root);
    assert!(!cache.join("views/working/index.fjall").exists());
    assert!(basemind::store_base::has_published_dirty_worktree(root, &config).unwrap());
    for (args, expected) in [
        (vec!["code", "files", "--path-contains", "sample.rs"], "sample.rs"),
        (vec!["code", "symbols", "after_symbol"], "after_symbol"),
    ] {
        let query = Command::new(bin())
            .env("BASEMIND_INDEX_LAYERED", "1")
            .args(["--root", root.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        assert!(query.status.success(), "{}", String::from_utf8_lossy(&query.stderr));
        assert!(
            String::from_utf8_lossy(&query.stdout).contains(expected),
            "{:?}",
            query.stdout
        );
    }
    assert!(!cache.join("views/working/index.fjall").exists());
    let rescan = || {
        Command::new(bin())
            .env("BASEMIND_INDEX_LAYERED", "1")
            .args(["--root", root.to_str().unwrap(), "rescan", "sample.rs"])
            .output()
            .unwrap()
    };
    git(root, &["checkout", "--", "sample.rs"]);
    let clean = rescan();
    assert!(clean.status.success(), "{}", String::from_utf8_lossy(&clean.stderr));
    assert!(
        String::from_utf8_lossy(&clean.stderr).contains("Shared worktree index ready"),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(!cache.join("views/working/index.fjall").exists());
    std::fs::write(root.join("sample.rs"), "fn after_symbol() { after(); }").unwrap();
    let dirty = rescan();
    assert!(dirty.status.success(), "{}", String::from_utf8_lossy(&dirty.stderr));
    assert!(!cache.join("views/working/index.fjall").exists());
    let api = |layered: &str, args: &[&str]| {
        let query = Command::new(bin())
            .env("BASEMIND_INDEX_LAYERED", layered)
            .args(["--json", "--root", root.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        assert!(query.status.success(), "{}", String::from_utf8_lossy(&query.stderr));
        let mut response: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
        let fields = response.as_object_mut().unwrap();
        fields.remove("elapsed_us");
        fields.remove("startup_us");
        response
    };
    let requests: &[(&[&str], &str)] = &[
        (
            &[
                "graph",
                "calls",
                "after_symbol",
                "--direction",
                "callees",
                "--path",
                "sample.rs",
            ],
            "after",
        ),
        (&["git", "status"], "sample.rs"),
        (&["git", "recent", "--limit", "1"], "fixture"),
    ];
    let layered = requests
        .iter()
        .map(|(args, expected)| {
            let result = api("1", args);
            assert!(result.to_string().contains(expected), "{result}");
            result
        })
        .collect::<Vec<_>>();
    let reference = Command::new(bin())
        .env("BASEMIND_INDEX_LAYERED", "0")
        .args(["--root", root.to_str().unwrap(), "scan", "--no-git-history"])
        .output()
        .unwrap();
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    for ((args, _), expected) in requests.iter().zip(layered) {
        assert_eq!(api("0", args), expected, "request {args:?}");
    }
    let full = Command::new(bin())
        .env("BASEMIND_INDEX_LAYERED", "1")
        .args(["--root", root.to_str().unwrap(), "rescan", "--full", "--no-git-history"])
        .output()
        .unwrap();
    assert!(full.status.success(), "{}", String::from_utf8_lossy(&full.stderr));
    assert!(!String::from_utf8_lossy(&full.stderr).contains("Shared worktree index ready"));
    for number in 0..2 {
        std::fs::write(root.join(format!("overflow_{number}.rs")), "fn overflow() {}").unwrap();
    }
    let opted_in_scan = || {
        Command::new(bin())
            .env("BASEMIND_INDEX_LAYERED", "1")
            .args(["--root", root.to_str().unwrap(), "scan", "--no-git-history"])
            .output()
            .unwrap()
    };
    assert!(opted_in_scan().status.success());
    assert!(cache.join("layered-promoted").is_file());
    for number in 0..2 {
        std::fs::remove_file(root.join(format!("overflow_{number}.rs"))).unwrap();
    }
    assert!(opted_in_scan().status.success());
    assert!(cache.join("layered-promoted").is_file());
    assert!(!basemind::store_base::has_published_dirty_worktree(root, &config).unwrap());
}
