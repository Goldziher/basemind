//! Upgrade path: an index built by an OLDER released binary, opened by this build against the SAME
//! data home.
//!
//! The old binaries come from `BASEMIND_UPGRADE_FROM_BIN` (a path list: `:` separated on Unix, `;` on
//! Windows) or from the plugin launcher's per-version cache
//! (`$XDG_CACHE_HOME|~/.cache/basemind/bin/<version>/basemind`, written by launchers up to 0.33;
//! later launchers install under `versions/`, which holds no release this test drives). CI downloads
//! checksum-verified release binaries into the variable (`scripts/fetch-release-binaries.sh`). With
//! no binary the test
//! checks nothing: locally it says so in a `SKIP:` line on stderr that the harness does not capture;
//! under `CI` it fails, so a pipeline that stops supplying binaries cannot go green. A
//! `BASEMIND_UPGRADE_FROM_BIN` entry that names no file fails it either way.
//!
//! The index shape changed once inside the supported window: through 0.28 the code map held prose,
//! data and config files; from 0.29 it holds code only and those files live in the document tier
//! (measured against the published binaries: 0.27.0, 0.27.4 and 0.28.0 outline `README.md`, 0.29.0
//! through 0.33.0 refuse it). Each old binary is therefore checked against the premise its release
//! actually has, then the upgrade is asserted:
//!
//! - an old release that code-maps prose has it migrated to the document tier, and the migrating
//!   scan reports what it reclaimed;
//! - prose, data and config files are out of the code map and code files stay in it;
//! - symbol signatures are the short, header-only form;
//! - no orphan blobs remain;
//! - a second rescan is a no-op and the blob set stops changing;
//! - the git history, a memory note and an agents thread the old release wrote are still answered;
//! - opening the upgraded data with the old binary again fails safe: it holds exactly the paths it
//!   held before and still answers `code symbols` from its blobs, and a re-upgrade converges (a
//!   second scan updates nothing, blob accounting holds).
//!
//! A second test leaves the newest old release's daemon running through the upgrade, as a plugin
//! update does, and checks that the new build's blob GC leaves the blobs that release reads while
//! it runs and reaps them once it stops. `BASEMIND_UPGRADE_REQUIRE_CROSS_MINOR` (set by the CI job
//! that bumps the tree to the next minor first) requires every release upgraded from to be of an
//! older minor.

#![cfg(feature = "documents")]

use std::fs;
use std::io::Write;
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

/// Env var naming the old release binaries to upgrade from (a path list), instead of the
/// launcher's cache.
const PIN_ENV: &str = "BASEMIND_UPGRADE_FROM_BIN";

/// The blob GC with no safety margin: an unreferenced blob goes at once
/// (`BASEMIND_BLOB_GC_GRACE_SECS`), and so does an older release's blob namespace as soon as no
/// process of that release runs (`BASEMIND_PRUNE_AFTER_DAYS`), as once the launcher's retention of
/// that release has passed.
const NO_MARGIN: [(&str, &str); 2] = [("BASEMIND_BLOB_GC_GRACE_SECS", "0"), ("BASEMIND_PRUNE_AFTER_DAYS", "0")];

/// Last release minor that keeps its blobs directly in the blobs root; later ones keep them in a
/// namespace of their own, `v<minor>/`.
#[cfg(feature = "comms")]
const LAST_UNNAMESPACED_MINOR: u32 = 33;

/// First release minor on the comms v2 protocol (`basemind agent ...`): its threads are in the
/// store this build reads, where an older release's `agents` store is a generation apart.
const FIRST_COMMS_V2_MINOR: u32 = 34;

/// Set by the CI job that bumps the tree to the next minor before building: every release upgraded
/// from must then be of an older minor, so the wipe-and-rebuild of a minor bump is what ran.
const CROSS_MINOR_ENV: &str = "BASEMIND_UPGRADE_REQUIRE_CROSS_MINOR";

/// Oldest release minor whose CLI shape this test drives (`--root`, `--json`, `code files`).
const OLDEST_MINOR: u32 = 27;

/// First release minor whose code map holds code only; older ones also map prose, data and config.
const FIRST_CODE_ONLY_MINOR: u32 = 29;

/// Minor of the build under test: a release of the same minor is the newest one to upgrade from.
fn current_minor() -> u32 {
    env!("CARGO_PKG_VERSION")
        .split('.')
        .nth(1)
        .and_then(|minor| minor.parse().ok())
        .expect("CARGO_PKG_VERSION has a minor component")
}

/// A command running `bin`. A Linux release loads the libraries bundled in its `lib/` through its
/// RUNPATH, which `LD_LIBRARY_PATH` outranks: under a job that exports one for this build's own
/// libraries, the loader mixes the two sets and refuses to start it. So a release runs without the
/// variable; this build, which may need it, keeps it.
fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    if bin != Path::new(NEW_BIN) {
        cmd.env_remove("LD_LIBRARY_PATH");
    }
    cmd
}

/// `basemind --version` prints `basemind X.Y.Z`; this is Y. Run in `env`'s sandbox, so not even
/// `--version` of an old release can reach a real data home or daemon.
fn release_minor(env: &Env, bin: &Path) -> u32 {
    let out = env.run(bin, &["--version"], &[]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.split_whitespace()
        .nth(1)
        .and_then(|version| version.split('.').nth(1))
        .and_then(|minor| minor.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "{} --version printed {text:?}, expected `basemind X.Y.Z`",
                bin.display()
            )
        })
}

fn old_binaries() -> Vec<PathBuf> {
    if let Some(pins) = std::env::var_os(PIN_ENV) {
        let pins: Vec<PathBuf> = std::env::split_paths(&pins)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        assert!(!pins.is_empty(), "{PIN_ENV} is set but names no binary");
        for pin in &pins {
            assert!(
                pin.is_file(),
                "{PIN_ENV} entry {} is not a file: the upgrade it asks for cannot be exercised",
                pin.display()
            );
        }
        return pins;
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
            let name = entry.file_name().to_string_lossy().into_owned();
            name.split('.')
                .nth(1)
                .and_then(|minor| minor.parse::<u32>().ok())
                .is_some_and(|minor| (OLDEST_MINOR..=current_minor()).contains(&minor))
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
    /// Binaries whose daemons are stopped when the test ends, pass or fail.
    daemon_bins: Vec<PathBuf>,
}

impl Drop for Env {
    fn drop(&mut self) {
        for bin in &self.daemon_bins {
            let _ = self.run(bin, &["comms", "stop"], &[]);
        }
    }
}

impl Env {
    /// Run `bin` against the sandbox with `vars` added to its environment.
    fn run(&self, bin: &Path, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut cmd = command(bin);
        cmd.args(["--root"])
            .arg(&self.repo)
            .args(["--no-color"])
            .args(args)
            .env("BASEMIND_DATA_HOME", &self.data)
            .env("BASEMIND_COMMS_DIR", &self.comms)
            .env("NO_COLOR", "1")
            .envs(vars.iter().copied());
        cmd.output().expect("spawn basemind")
    }

    fn ok(&self, bin: &Path, args: &[&str], vars: &[(&str, &str)]) -> String {
        let out = self.run(bin, args, vars);
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
        let out = self.run(bin, &full, &[]);
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
    /// upgrade does. The scan's blob GC runs with [`NO_MARGIN`]: nothing of the old release runs.
    fn scan_after_stopping_daemons(&self, old: &Path, new: &Path) -> String {
        for bin in [old, new] {
            let _ = self.run(bin, &["comms", "stop"], &[]);
        }
        // A writer can outlive the stop for a moment: a daemon still draining, or a rescan an old
        // release started in the background. `scan` names that writer and exits; wait it out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let out = self.run(new, &["scan"], &NO_MARGIN);
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).into_owned();
            }
            assert!(
                stderr.contains("already running against this repo") && std::time::Instant::now() < deadline,
                "scan failed: {stderr}"
            );
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// The `(path, name)` of each hit an old release's `code symbols <name>` reports under exactly
    /// that name. Releases up to 0.33 list `{"path", "name", ...}` objects.
    fn old_symbol_hits(&self, old: &Path, name: &str) -> Vec<(String, String)> {
        self.json(old, &["code", "symbols", name])["results"]
            .as_array()
            .map(|hits| {
                hits.iter()
                    .filter(|hit| hit["name"] == name)
                    .map(|hit| (hit["path"].as_str().unwrap_or_default().to_string(), name.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The symbol names an old release's outline of `path` lists, read from the file's blob.
    #[cfg(feature = "comms")]
    fn old_outline_names(&self, old: &Path, path: &str) -> Vec<String> {
        self.json(old, &["code", "outline", path])["symbols"]
            .as_array()
            .map(|symbols| {
                symbols
                    .iter()
                    .filter_map(|symbol| symbol["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the blob store holds blobs where a release of `minor` keeps them: directly in the
    /// blobs root up to [`LAST_UNNAMESPACED_MINOR`], in `v<minor>/` after it.
    #[cfg(feature = "comms")]
    fn holds_blobs_of(&self, minor: u32) -> bool {
        let root = self.data.join("cache").join("blobs");
        let dir = if minor <= LAST_UNNAMESPACED_MINOR {
            root
        } else {
            root.join(format!("v{minor}"))
        };
        fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".msgpack"))
        })
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
            // ~keep A release before lean rows lists `{"path": ...}` objects; this build lists the paths.
            .map(|files| {
                files
                    .iter()
                    .filter_map(|f| f.as_str().or_else(|| f["path"].as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        listed
            .into_iter()
            .filter(|path| {
                self.run(bin, &["--json", "code", "outline", path], &[])
                    .status
                    .success()
            })
            .collect()
    }
}

/// Run `git` in `repo` as the fixture's committer.
fn git(repo: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(["-c", "user.name=upgrade", "-c", "user.email=upgrade@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn fixture(tmp: &Path, daemon_bins: &[&Path]) -> Env {
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
    // Two real commits, so the git history every version indexes has something to carry over.
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "first upgrade fixture commit"]);
    write(
        "src/lib.rs",
        "pub fn alpha(x: u32) -> u32 {\n    let y = x + 2;\n    y * 2\n}\n",
    );
    git(&repo, &["commit", "-qam", "second upgrade fixture commit"]);
    let data = tmp.join("data");
    let comms = tmp.join("comms");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&comms).unwrap();
    Env {
        repo,
        data,
        comms,
        daemon_bins: daemon_bins.iter().map(|bin| bin.to_path_buf()).collect(),
    }
}

/// Puts something in every store an old release writes besides the code map: a memory note and
/// an agents thread. The git history is in the fixture already.
fn populate_old_stores(env: &Env, old: &Path) {
    if cfg!(feature = "memory") {
        env.ok(old, &["memory", "put", "upgrade/probe", "kept across upgrades"], &[]);
    }
    if cfg!(feature = "comms") {
        if release_minor(env, old) >= FIRST_COMMS_V2_MINOR {
            env.ok(
                old,
                &[
                    "agent",
                    "post",
                    "--subject",
                    "upgrade-probe",
                    "--paths",
                    "src/**",
                    "--body",
                    "kept across upgrades",
                ],
                &[],
            );
        } else {
            env.ok(
                old,
                &[
                    "agents",
                    "thread-start",
                    "--subject",
                    "upgrade-probe",
                    "--path",
                    "src/**",
                ],
                &[],
            );
        }
    }
}

/// What the new build must still answer from the stores [`populate_old_stores`] filled.
fn assert_old_stores_survive(env: &Env, new: &Path, old: &Path) {
    let log = env.json(new, &["git", "log"]);
    let summaries: Vec<&str> = log["commits"]
        .as_array()
        .map(|commits| commits.iter().filter_map(|row| row["subject"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        summaries.iter().any(|s| s.contains("second upgrade fixture commit"))
            && summaries.iter().any(|s| s.contains("first upgrade fixture commit")),
        "{}: git history after the upgrade: {log}",
        old.display()
    );
    if cfg!(feature = "memory") {
        let note = env.json(new, &["memory", "get", "upgrade/probe"]);
        assert!(
            note.to_string().contains("kept across upgrades"),
            "{}: a memory note written by the old release is gone after the upgrade: {note}",
            old.display()
        );
    }
    if cfg!(feature = "comms") {
        // ADR-0021 rule 12: a new protocol generation starts with an empty store and does not
        // import the old one, whose `store.fjall` is left untouched. The old store must neither
        // stop this build from answering nor be deleted by it.
        let threads = env.json(new, &["agent", "threads"]);
        if release_minor(env, old) >= FIRST_COMMS_V2_MINOR {
            assert!(
                threads.to_string().contains("upgrade-probe"),
                "{}: a thread of the same protocol generation is gone after the upgrade: {threads}",
                old.display()
            );
        } else {
            assert!(
                !threads.to_string().contains("upgrade-probe"),
                "{}: a thread of the legacy store was imported: {threads}",
                old.display()
            );
            assert!(
                env.comms.join("store.fjall").is_dir(),
                "{}: the upgrade removed the old release's comms store",
                old.display()
            );
        }
    }
}

/// Upgrades one old release to this build and returns whether that release mapped prose into the
/// code map (the migration path) rather than already keeping it in the document tier.
fn upgrade_from(old: &Path) -> (bool, u32) {
    let tmp = tempfile::tempdir().unwrap();
    let new = Path::new(NEW_BIN);
    let env = fixture(tmp.path(), &[old, new]);
    let minor = release_minor(&env, old);
    let maps_prose = minor < FIRST_CODE_ONLY_MINOR;

    env.ok(old, &["scan"], &[]);
    populate_old_stores(&env, old);
    let before = env.code_paths(old);
    for name in NON_CODE {
        assert_eq!(
            before.iter().any(|p| p == name),
            maps_prose,
            "{}: release 0.{minor} {} {name} in its code map, got {before:?}",
            old.display(),
            if maps_prose { "should hold" } else { "should not hold" }
        );
    }
    for code in ["src/lib.rs", "src/app.py", "src/main.ts"] {
        assert!(
            before.iter().any(|p| p == code),
            "{}: the old release code-maps {code}, got {before:?}",
            old.display()
        );
    }

    // --- upgrade: first scan with the new build -------------------------------------------------
    let first = env.scan_after_stopping_daemons(old, new);
    if maps_prose {
        assert!(
            first.contains("cleanup: reclaimed"),
            "{}: the migrating scan reports what it reclaimed:\n{first}",
            old.display()
        );
    }

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
    let signature = symbols["results"][0]
        .as_str()
        .expect("a row: `<path>:<line>: <signature>`");
    assert!(
        !signature.contains("def run"),
        "signature must stop at the class header, not embed the body: {signature:?}"
    );
    // `code_grep` searches the document tier's text files too: the migrated README is found once,
    // from there, and not a second time from a code-map entry the old release left behind.
    // `hits` is `{"<path>": {"<symbol>": ["<line>: <text>", ...]}}`; `total` only beside a cursor.
    let usage = env.json(new, &["code", "grep", "Usage"]);
    let files = usage["hits"].as_object().expect("hits grouped by file");
    assert_eq!(
        files.keys().collect::<Vec<_>>(),
        ["README.md"],
        "the hit is the README: {usage}"
    );
    let rows: usize = files["README.md"]
        .as_object()
        .expect("a file's hits grouped by symbol")
        .values()
        .map(|rows| rows.as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(rows, 1, "README is grepped once: {usage}");
    assert_eq!(
        env.json(new, &["code", "symbols", "Project"])["results"],
        serde_json::json!([]),
        "no markdown headings as symbols"
    );

    let stats = env.json(new, &["cache", "stats"]);
    assert_eq!(stats["blob_accounting_ok"], true);
    assert_eq!(
        stats["orphan_blob_count"], 0,
        "no orphan blobs left after the upgrade: {stats}"
    );
    assert_old_stores_survive(&env, new, old);

    // --- converge: a second scan is a no-op and the blob set is stable --------------------------
    let blobs_after_first = env.blob_names();
    let second = env.scan_after_stopping_daemons(old, new);
    assert!(second.contains("updated 0"), "second scan is a no-op:\n{second}");
    assert!(!second.contains("tier_migrated"), "nothing left to migrate:\n{second}");
    assert!(!second.contains("cleanup:"), "nothing left to reclaim:\n{second}");
    assert_eq!(env.blob_names(), blobs_after_first, "the blob set stops changing");

    // --- downgrade fails safe: the old release reads the upgraded data back to its own shape -----
    for bin in [old, new] {
        let _ = env.run(bin, &["comms", "stop"], &[]);
    }
    env.ok(old, &["scan"], &[]);
    let mut downgraded = env.code_paths(old);
    let mut expected = before.clone();
    downgraded.sort();
    expected.sort();
    assert_eq!(
        downgraded,
        expected,
        "{}: after a downgrade the old release must hold exactly the paths it held before the upgrade",
        old.display()
    );
    assert_eq!(
        env.old_symbol_hits(old, "alpha"),
        [("src/lib.rs".to_string(), "alpha".to_string())],
        "{}: after a downgrade the old release still answers from its blobs",
        old.display()
    );

    // --- re-upgrade converges again -------------------------------------------------------------
    env.scan_after_stopping_daemons(old, new);
    let again = env.scan_after_stopping_daemons(old, new);
    assert!(again.contains("updated 0"), "re-upgrade converges:\n{again}");
    let stats = env.json(new, &["cache", "stats"]);
    assert_eq!(stats["blob_accounting_ok"], true, "re-upgrade accounting: {stats}");
    assert_eq!(stats["orphan_blob_count"], 0, "re-upgrade converges again: {stats}");
    let mut code = env.code_paths(new);
    code.sort();
    assert_eq!(code, ["src/app.py", "src/lib.rs", "src/main.ts"]);
    (maps_prose, minor)
}

#[test]
fn upgrading_from_cached_releases_migrates_cleans_and_converges() {
    let olds = old_binaries();
    if olds.is_empty() {
        assert!(
            std::env::var_os("CI").is_none(),
            "no old release binary under CI: {PIN_ENV} is unset and the launcher cache is empty, so the upgrade \
             path was not exercised. The CI step that downloads release binaries did not run."
        );
        // ~keep Written to the stream itself: the harness captures `eprintln!` of a passing test.
        let _ = writeln!(
            std::io::stderr().lock(),
            "SKIP: upgrading_from_cached_releases_migrates_cleans_and_converges checked nothing: no \
             release 0.{OLDEST_MINOR} to 0.{} under $XDG_CACHE_HOME|~/.cache/basemind/bin/<version>/basemind and \
             {PIN_ENV} is unset. Set {PIN_ENV} to the `basemind` binary of such a release to run it.",
            current_minor()
        );
        return;
    }
    let mut migrated = 0;
    let mut cross_minor = 0;
    for old in &olds {
        let (migrates, minor) = upgrade_from(old);
        migrated += usize::from(migrates);
        cross_minor += usize::from(minor < current_minor());
        let _ = writeln!(
            std::io::stderr().lock(),
            "upgrade-from-release: {} (0.{minor}) upgraded to 0.{}, {}",
            old.display(),
            current_minor(),
            if migrates {
                "prose migrated to the document tier"
            } else {
                "code-only index kept"
            }
        );
    }
    if std::env::var_os(CROSS_MINOR_ENV).is_some() {
        assert!(
            cross_minor == olds.len(),
            "{CROSS_MINOR_ENV} is set, but {} of the {} releases upgraded from share this build's minor 0.{}: \
             the release-minor bump was not exercised",
            olds.len() - cross_minor,
            olds.len(),
            current_minor()
        );
    }
    if std::env::var_os("CI").is_some() {
        assert!(
            migrated > 0,
            "CI exercised no release that code-maps prose, so the migration was not checked"
        );
        assert!(
            migrated < olds.len(),
            "CI exercised no release of the code-only shape, so the same-shape upgrade was not checked"
        );
    }
}

/// A real plugin update does not stop the daemon an older release left running: the new build's
/// first scan and reads meet it. The newest release given runs its daemon throughout, serving a
/// second repository the new build never scans. The new build's blob GC, with no grace and no
/// hold, must leave the blobs that daemon reads, and reap them once it is gone.
#[cfg(feature = "comms")]
#[test]
fn upgrading_while_the_old_release_s_daemon_still_runs() {
    let olds = old_binaries();
    let Some(old) = olds.last() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "no old release binary under CI: {PIN_ENV} is unset"
        );
        let _ = writeln!(
            std::io::stderr().lock(),
            "SKIP: upgrading_while_the_old_release_s_daemon_still_runs checked nothing: {PIN_ENV} is unset"
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let new = Path::new(NEW_BIN);
    let env = fixture(tmp.path(), &[old.as_path(), new]);
    let other = Env {
        repo: tmp.path().join("other"),
        data: env.data.clone(),
        comms: env.comms.clone(),
        daemon_bins: Vec::new(),
    };
    fs::create_dir_all(other.repo.join("src")).unwrap();
    fs::write(
        other.repo.join("src/lib.rs"),
        "pub fn beta(x: u32) -> u32 {\n    x * 3\n}\n",
    )
    .unwrap();
    fs::copy(env.repo.join("basemind.toml"), other.repo.join("basemind.toml")).unwrap();
    git(&other.repo, &["init", "-q"]);
    git(&other.repo, &["add", "-A"]);
    git(&other.repo, &["commit", "-qm", "the other repository"]);
    env.ok(old, &["scan"], &[]);
    other.ok(old, &["scan"], &[]);
    env.ok(old, &["comms", "start"], &[]);
    env.ok(old, &["comms", "status"], &[]);

    // The old daemon is never stopped. A writer it runs may hold the workspace for a moment; the
    // new scan names it and exits 3, and is retried, but it must get through.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let out = env.run(new, &["scan"], &NO_MARGIN);
        if out.status.success() {
            break;
        }
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.code() == Some(3) && std::time::Instant::now() < deadline,
            "{}: the new scan did not get past the old release's running daemon: {stderr}",
            old.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let gc = env.run(new, &["cache", "gc"], &NO_MARGIN);
    assert!(gc.status.success(), "cache gc: {}", String::from_utf8_lossy(&gc.stderr));
    let mut code = env.code_paths(new);
    code.sort();
    assert_eq!(
        code,
        ["src/app.py", "src/lib.rs", "src/main.ts"],
        "{}: the new build reads its own index while the old daemon runs",
        old.display()
    );
    let log = env.json(new, &["git", "log"]);
    assert!(
        log.to_string().contains("second upgrade fixture commit"),
        "git history next to the old daemon: {log}"
    );

    // The old release, its daemon still up, answers for the repository the new build never saw.
    // A release of this build's minor shares its namespace, which the superseded sweep never takes.
    let minor = release_minor(&env, old);
    let superseded = minor <= LAST_UNNAMESPACED_MINOR || minor < current_minor();
    assert!(
        env.holds_blobs_of(minor),
        "{}: the GC reaped the blobs of a running release",
        old.display()
    );
    assert_eq!(
        other.old_outline_names(old, "src/lib.rs"),
        ["beta"],
        "{}: the old release outlines from its blobs after the new build's GC",
        old.display()
    );
    assert_eq!(
        other.old_symbol_hits(old, "beta"),
        [("src/lib.rs".to_string(), "beta".to_string())],
        "{}: the old release finds its symbols after the new build's GC",
        old.display()
    );

    // Once no process of that release runs, the same GC reaps what it left.
    if !superseded {
        return;
    }
    let _ = env.run(old, &["comms", "stop"], &[]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while env.holds_blobs_of(minor) && std::time::Instant::now() < deadline {
        let _ = env.run(new, &["cache", "gc"], &NO_MARGIN);
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert!(
        !env.holds_blobs_of(minor),
        "{}: the GC kept the blobs of a release that is no longer running",
        old.display()
    );
}
