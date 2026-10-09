//! Upgrade path: an index built by an OLDER released binary, opened by this build against the SAME
//! data home.
//!
//! The old binary comes from the plugin launcher's per-version cache
//! (`$XDG_CACHE_HOME|~/.cache/basemind/bin/<version>/basemind`) or from `BASEMIND_UPGRADE_FROM_BIN`.
//! When none is present the test skips cleanly (CI has no cached releases), so it is a developer /
//! release-gate check, not a hermetic one. Per old binary it asserts that:
//!
//! - prose, data and config files left the code map and live only in the document tier;
//! - symbol signatures are the short, header-only form;
//! - the blobs and rows the old shape left behind are reclaimed (no orphan blobs);
//! - a second rescan is a no-op and the blob set stops changing;
//! - opening the upgraded data with the old binary again fails safe (it works, never corrupts) and
//!   converges once more on re-upgrade.

#![cfg(feature = "documents")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const NEW_BIN: &str = env!("CARGO_BIN_EXE_basemind");

/// Prose / data / config files the old binary code-mapped and this build routes to documents.
const NON_CODE: &[&str] = &[
    "README.md",
    "docs/guide.md",
    "config.json",
    "settings.yaml",
    "Cargo.toml",
];

fn old_binaries() -> Vec<PathBuf> {
    if let Some(pin) = std::env::var_os("BASEMIND_UPGRADE_FROM_BIN") {
        let pin = PathBuf::from(pin);
        return if pin.is_file() { vec![pin] } else { Vec::new() };
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    let Some(bin_root) = cache.map(|c| c.join("basemind").join("bin")) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&bin_root) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            // 0.27 is the oldest CLI shape this test drives (`--root`, `--json`, `code files`).
            let name = entry.file_name().to_string_lossy().into_owned();
            name.split('.')
                .nth(1)
                .and_then(|minor| minor.parse::<u32>().ok())
                .is_some_and(|minor| minor >= 27)
                && name.starts_with("0.")
        })
        .map(|entry| entry.path().join("basemind"))
        .filter(|bin| bin.is_file())
        .collect();
    found.sort();
    found
}

struct Env {
    repo: PathBuf,
    data: PathBuf,
    comms: PathBuf,
}

impl Env {
    fn run(&self, bin: &Path, args: &[&str], grace_secs: Option<&str>) -> Output {
        let mut cmd = Command::new(bin);
        cmd.args(["--root"])
            .arg(&self.repo)
            .args(["--no-color"])
            .args(args)
            .env("BASEMIND_DATA_HOME", &self.data)
            .env("BASEMIND_COMMS_DIR", &self.comms)
            .env("NO_COLOR", "1");
        if let Some(grace) = grace_secs {
            cmd.env("BASEMIND_BLOB_GC_GRACE_SECS", grace);
        }
        cmd.output().expect("spawn basemind")
    }

    fn ok(&self, bin: &Path, args: &[&str], grace_secs: Option<&str>) -> String {
        let out = self.run(bin, args, grace_secs);
        assert!(
            out.status.success(),
            "{} {args:?} failed:\n{}",
            bin.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, bin: &Path, args: &[&str]) -> Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.run(bin, &full, None);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Log lines may precede the document on stdout; the JSON object starts at the first `{`.
        let start = stdout.find('{').expect("a JSON document on stdout");
        serde_json::from_str(&stdout[start..]).expect("valid JSON")
    }

    /// A new `scan` refuses to run next to a writer (exit 3), and any command of an old release may
    /// have auto-spawned a daemon that holds the lock, so stop every daemon first, as a real
    /// upgrade does.
    fn scan_after_stopping_daemons(&self, old: &Path, new: &Path) -> String {
        for bin in [old, new] {
            let _ = self.run(bin, &["comms", "stop"], None);
        }
        self.ok(new, &["scan"], Some("0"))
    }

    fn blob_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.data.join("cache").join("blobs"))
            .map(|dir| {
                dir.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Paths the code map itself holds. `code files` also lists the document tier on current
    /// builds, so each candidate is confirmed by asking for its outline, which only a code-mapped
    /// file can answer.
    fn code_paths(&self, bin: &Path) -> Vec<String> {
        let listed: Vec<String> = self.json(bin, &["code", "files"])["files"]
            .as_array()
            .map(|files| {
                files
                    .iter()
                    .filter_map(|f| f["path"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        listed
            .into_iter()
            .filter(|path| {
                self.run(bin, &["--json", "code", "outline", path], None)
                    .status
                    .success()
            })
            .collect()
    }
}

fn fixture(tmp: &Path) -> Env {
    let repo = tmp.join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("docs")).unwrap();
    let write = |rel: &str, body: &str| fs::write(repo.join(rel), body).unwrap();
    write(
        "src/lib.rs",
        "pub fn alpha(x: u32) -> u32 {\n    let y = x + 1;\n    y * 2\n}\n",
    );
    write(
        "src/app.py",
        "def compute(a, b):\n    total = 0\n    for i in range(a):\n        total += i * b\n    return total\n\n\
         class Svc:\n    def run(self, n):\n        return compute(n, 2)\n",
    );
    write(
        "src/main.ts",
        "export function greet(name: string): string {\n  const msg = `hello ${name}`;\n  return msg;\n}\n",
    );
    write(
        "README.md",
        "# Project\n\nSome prose about the widget project.\n\n## Usage\n\nRun alpha.\n",
    );
    write("docs/guide.md", "# Guide\n\nHow to use compute.\n");
    write("config.json", "{\"name\":\"demo\",\"version\":\"1.0.0\"}\n");
    write("settings.yaml", "name: demo\nitems:\n  - alpha\n  - beta\n");
    write("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n");
    // Keeps the new build from loading an embedding model: the test is about index shape, not vectors.
    write("basemind.toml", "\"$schema\" = \"v1\"\n\n[documents]\nembed = false\n");
    let _ = Command::new("git").args(["init", "-q"]).current_dir(&repo).status();
    let data = tmp.join("data");
    let comms = tmp.join("comms");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&comms).unwrap();
    Env { repo, data, comms }
}

fn upgrade_from(old: &Path) {
    let tmp = tempfile::tempdir().unwrap();
    let env = fixture(tmp.path());
    let new = Path::new(NEW_BIN);

    env.ok(old, &["scan"], None);
    let before = env.code_paths(old);
    for name in NON_CODE {
        assert!(
            before.iter().any(|p| p == name),
            "{}: the old release code-maps {name} (premise of the test), got {before:?}",
            old.display()
        );
    }

    // --- upgrade: first scan with the new build -------------------------------------------------
    let first = env.scan_after_stopping_daemons(old, new);
    assert!(
        first.contains("cleanup: reclaimed"),
        "{}: the migrating scan reports what it reclaimed:\n{first}",
        old.display()
    );

    let after = env.code_paths(new);
    for name in NON_CODE {
        assert!(
            !after.iter().any(|p| p == name),
            "{name} must have left the code map: {after:?}"
        );
    }
    for code in ["src/lib.rs", "src/app.py", "src/main.ts"] {
        assert!(after.iter().any(|p| p == code), "{code} stays code-mapped: {after:?}");
    }

    let symbols = env.json(new, &["code", "symbols", "Svc"]);
    let signature = symbols["results"][0]["signature"].as_str().expect("a signature");
    assert!(
        !signature.contains("def run"),
        "signature must stop at the class header, not embed the body: {signature:?}"
    );
    assert_eq!(
        env.json(new, &["code", "grep", "Usage"])["total_matches"],
        0,
        "README is not in the code grep"
    );
    assert_eq!(
        env.json(new, &["code", "symbols", "Project"])["total"],
        0,
        "no markdown headings as symbols"
    );

    let stats = env.json(new, &["cache", "stats"]);
    assert_eq!(stats["blob_accounting_ok"], true);
    assert_eq!(
        stats["orphan_blob_count"], 0,
        "no orphan blobs left after the upgrade: {stats}"
    );

    // --- converge: a second scan is a no-op and the blob set is stable --------------------------
    let blobs_after_first = env.blob_names();
    let second = env.scan_after_stopping_daemons(old, new);
    assert!(second.contains("updated 0"), "second scan is a no-op:\n{second}");
    assert!(!second.contains("tier_migrated"), "nothing left to migrate:\n{second}");
    assert!(!second.contains("cleanup:"), "nothing left to reclaim:\n{second}");
    assert_eq!(env.blob_names(), blobs_after_first, "the blob set stops changing");

    // --- downgrade fails safe -------------------------------------------------------------------
    env.ok(old, &["scan"], None);
    assert!(
        !env.code_paths(old).is_empty(),
        "{}: the old binary still reads and rewrites the shared data",
        old.display()
    );
    env.scan_after_stopping_daemons(old, new);
    let stats = env.json(new, &["cache", "stats"]);
    assert_eq!(stats["orphan_blob_count"], 0, "re-upgrade converges again: {stats}");
    assert_eq!(env.code_paths(new).len(), 3);
}

#[test]
fn upgrading_from_cached_releases_migrates_cleans_and_converges() {
    let olds = old_binaries();
    if olds.is_empty() {
        eprintln!("skipping: no cached release under ~/.cache/basemind/bin and no BASEMIND_UPGRADE_FROM_BIN");
        return;
    }
    for old in olds {
        upgrade_from(&old);
    }
}
