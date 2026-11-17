//! Multi-worktree measurement mode of the hardening harness.
//!
//! For one cloned canary repository it adds K git worktrees at older commits, scans each, and
//! records what an added worktree costs: bytes on disk (journal, keyspaces, vectors and file map
//! separately), the time to open and first-scan it, and the share of that scan spent in the
//! resolve pass. It prints a table and appends it to `<results dir>/worktrees.md`.
//!
//! This is a measurement, so it asserts only that each number was really taken. Run it through
//! `scripts/harden.sh` with `BASEMIND_HARDEN_WORKTREES=<K>`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use basemind::scan_evidence::{self, PHASE_RESOLVE};
use basemind::store::{
    DATA_HOME_ENV, INDEX_FILE, Store, VIEW_WORKING, VIEWS_DIR, global_blobs_dir, workspace_cache_dir,
};

use super::{dir_size, human_bytes, on_disk_size};

/// Worktrees added when `BASEMIND_HARDEN_WORKTREES` does not parse as a count.
const DEFAULT_WORKTREES: usize = 5;
/// First-parent commits between consecutive worktrees, unless `BASEMIND_HARDEN_WORKTREE_STEP` says
/// otherwise. Clamped to the history the clone has.
const DEFAULT_COMMIT_STEP: usize = 50;
/// How often the scan's phase breadcrumb is read. A phase shorter than this can be missed.
const PHASE_POLL: Duration = Duration::from_micros(500);
/// Directory name of the Fjall index inside a view directory.
const INDEX_DB_DIR: &str = "index.fjall";
/// Directory name of the workspace's LanceDB store. The library exports it only on a build with
/// the vector features, and this mode also runs without them.
const LANCE_DIR: &str = "lance";

/// Allocated bytes of one workspace's cache directory, by what holds them.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
struct DiskBytes {
    /// Fjall journal files of the working view's index.
    journal: u64,
    /// Logical length of the same files: what a filesystem without sparse files would charge.
    journal_logical: u64,
    /// The rest of the working view's Fjall index.
    keyspaces: u64,
    /// The workspace's LanceDB store. Zero on a build without the vector features.
    vectors: u64,
    /// The working view's file map (`index.msgpack`).
    manifest: u64,
    /// Everything else under the workspace directory (git-history index, sidecars, lock).
    other: u64,
}

impl DiskBytes {
    fn total(&self) -> u64 {
        self.journal + self.keyspaces + self.vectors + self.manifest + self.other
    }
}

/// Allocated bytes of the Fjall journal files directly under `index`, and of the rest of `index`.
fn index_bytes(index: &Path) -> (u64, u64) {
    let journal: u64 = std::fs::read_dir(index)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jnl"))
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| on_disk_size(&meta))
        .sum();
    (journal, dir_size(index).saturating_sub(journal))
}

/// Journal and keyspace bytes of the group index of `root`.
fn group_index_bytes(root: &Path) -> Option<(u64, u64)> {
    Some(index_bytes(
        &basemind::store_layout::group_index_dir(root).join(INDEX_DB_DIR),
    ))
}

/// What the worktree's cache directory holds. In the group layout its index rows live in the group
/// index, so journal and keyspaces are what the group index grew by across the worktree's scan
/// (`group_before`, taken before it).
fn disk_bytes(cache_dir: &Path, root: &Path, group_before: Option<(u64, u64)>) -> DiskBytes {
    let view = cache_dir.join(VIEWS_DIR).join(VIEW_WORKING);
    let (own_journal, own_keyspaces) = index_bytes(&view.join(INDEX_DB_DIR));
    let (journal, keyspaces) = match (group_before, group_index_bytes(root)) {
        (Some((journal_before, keyspaces_before)), Some((journal_after, keyspaces_after))) => (
            journal_after.saturating_sub(journal_before),
            keyspaces_after.saturating_sub(keyspaces_before),
        ),
        _ => (own_journal, own_keyspaces),
    };
    let vectors = dir_size(&cache_dir.join(LANCE_DIR));
    let manifest = std::fs::metadata(view.join(INDEX_FILE)).map_or(0, |meta| on_disk_size(&meta));
    let other = dir_size(cache_dir).saturating_sub(own_journal + own_keyspaces + vectors + manifest);
    DiskBytes {
        journal,
        journal_logical: basemind::index::journal_bytes_on_disk(&view),
        keyspaces,
        vectors,
        manifest,
        other,
    }
}

#[derive(Debug, serde::Serialize)]
struct WorktreeRow {
    label: String,
    commit: String,
    /// Files that differ from the main checkout's `HEAD`.
    diff_files: usize,
    scanned: usize,
    updated: usize,
    reused_extraction: usize,
    skipped_unchanged: usize,
    /// `Store::open` on a workspace with no index yet.
    open_ms: f64,
    scan_ms: f64,
    /// Time in the resolve phase; `None` when the breadcrumb never showed it.
    resolve_ms: Option<f64>,
    /// `Store::open` again once the scan has been flushed and the store dropped.
    reopen_ms: f64,
    disk: DiskBytes,
    /// Growth of the global blob store across this worktree's scan.
    new_blob_bytes: u64,
}

impl WorktreeRow {
    fn resolve_share(&self) -> Option<f64> {
        self.resolve_ms
            .filter(|_| self.scan_ms > 0.0)
            .map(|ms| ms / self.scan_ms)
    }
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

/// Read the scan's phase breadcrumb until `stop`, returning each phase with the instant it was
/// first seen. The scan report carries no phase timings, so this is how a phase is timed from
/// outside the crate.
fn watch_phases(cache_dir: PathBuf, stop: Arc<AtomicBool>) -> Vec<(String, Instant)> {
    let mut seen: Vec<(String, Instant)> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if let Some(record) = scan_evidence::read(&cache_dir)
            && seen.last().is_none_or(|(phase, _)| *phase != record.phase)
        {
            seen.push((record.phase, Instant::now()));
        }
        std::thread::sleep(PHASE_POLL);
    }
    seen
}

/// Open, scan and reopen the worktree at `root`, which has no file map yet.
fn measure_worktree(root: &Path, label: &str, commit: &str, diff_files: usize) -> WorktreeRow {
    let cache_dir = workspace_cache_dir(root);
    let blobs_before = dir_size(&global_blobs_dir());
    let group_before = group_index_bytes(root);
    let mut config = basemind::config::load(root).unwrap_or_else(|_| basemind::config::default_for_root(root));
    config.documents.enabled = false;

    let open_started = Instant::now();
    let mut store = Store::open(root, VIEW_WORKING).expect("open worktree store");
    let open_ms = millis(open_started.elapsed());

    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let stop = Arc::clone(&stop);
        let cache_dir = cache_dir.clone();
        std::thread::spawn(move || watch_phases(cache_dir, stop))
    };
    let scan_started = Instant::now();
    let report = basemind::scanner::scan(
        root,
        &mut store,
        &config,
        basemind::scanner::ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .expect("scan worktree");
    let scan_ended = Instant::now();
    stop.store(true, Ordering::Relaxed);
    let phases = watcher.join().expect("phase watcher");
    drop(store);

    let resolve_ms = phases.iter().position(|(phase, _)| phase == PHASE_RESOLVE).map(|at| {
        let ended = phases.get(at + 1).map_or(scan_ended, |(_, instant)| *instant);
        millis(ended.duration_since(phases[at].1))
    });

    let reopen_started = Instant::now();
    drop(Store::open(root, VIEW_WORKING).expect("reopen worktree store"));
    let reopen_ms = millis(reopen_started.elapsed());

    WorktreeRow {
        label: label.to_string(),
        commit: commit.chars().take(10).collect(),
        diff_files,
        scanned: report.stats.scanned,
        updated: report.stats.updated,
        reused_extraction: report.stats.reused_extraction,
        skipped_unchanged: report.stats.skipped_unchanged,
        open_ms,
        scan_ms: millis(scan_ended.duration_since(scan_started)),
        resolve_ms,
        reopen_ms,
        disk: disk_bytes(&cache_dir, root, group_before),
        new_blob_bytes: dir_size(&global_blobs_dir()).saturating_sub(blobs_before),
    }
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `count` first-parent ancestors of `HEAD`, `step` commits apart, nearest first. Fewer when the
/// clone's history is shorter; the step shrinks before the count does.
fn older_commits(repo: &Path, count: usize, step: usize) -> Vec<String> {
    let limit = (count * step + 1).to_string();
    let listing = git(repo, &["rev-list", "--first-parent", "--max-count", &limit, "HEAD"]).unwrap_or_default();
    let history: Vec<&str> = listing.lines().skip(1).collect();
    if history.is_empty() || count == 0 {
        return Vec::new();
    }
    let step = step.min(history.len() / count).max(1);
    (1..=count)
        .filter_map(|n| history.get(n * step - 1))
        .map(|sha| (*sha).to_string())
        .collect()
}

fn env_count(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

fn render(repo_name: &str, rows: &[WorktreeRow]) -> String {
    let mut out = format!(
        "### {repo_name}: {} added worktrees, group index (journal and keyspaces: growth of the group index)\n\n",
        rows.len() - 1,
    );
    out.push_str(
        "| worktree | commit | files | diff vs HEAD | open ms | scan ms | resolve ms | resolve share | reopen ms \
         | journal | keyspaces | vectors | file map | other | total | new blobs |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for row in rows {
        let resolve = row.resolve_ms.map_or("n/a".to_string(), |ms| format!("{ms:.0}"));
        let share = row
            .resolve_share()
            .map_or("n/a".to_string(), |share| format!("{:.0}%", share * 100.0));
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.0} | {:.0} | {resolve} | {share} | {:.0} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.label,
            row.commit,
            row.scanned,
            row.diff_files,
            row.open_ms,
            row.scan_ms,
            row.reopen_ms,
            human_bytes(row.disk.journal),
            human_bytes(row.disk.keyspaces),
            human_bytes(row.disk.vectors),
            human_bytes(row.disk.manifest),
            human_bytes(row.disk.other),
            human_bytes(row.disk.total()),
            human_bytes(row.new_blob_bytes),
        ));
    }
    let added = &rows[1..];
    if !added.is_empty() {
        let mean = |pick: fn(&WorktreeRow) -> u64| added.iter().map(pick).sum::<u64>() / added.len() as u64;
        out.push_str(&format!(
            "\nPer added worktree (mean of {}): {} total = journal {} + keyspaces {} + vectors {} + file map {} + \
             other {}; journal logical length {}; open {:.0} ms, first scan {:.0} ms.\n\n",
            added.len(),
            human_bytes(mean(|row| row.disk.total())),
            human_bytes(mean(|row| row.disk.journal)),
            human_bytes(mean(|row| row.disk.keyspaces)),
            human_bytes(mean(|row| row.disk.vectors)),
            human_bytes(mean(|row| row.disk.manifest)),
            human_bytes(mean(|row| row.disk.other)),
            human_bytes(mean(|row| row.disk.journal_logical)),
            added.iter().map(|row| row.open_ms).sum::<f64>() / added.len() as f64,
            added.iter().map(|row| row.scan_ms).sum::<f64>() / added.len() as f64,
        ));
    }
    out
}

fn append(path: &Path, text: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = write!(file, "{text}");
    }
}

#[test]
#[ignore = "multi-worktree measurement; invoke via scripts/harden.sh with BASEMIND_HARDEN_WORKTREES=<K>"]
fn harden_worktrees() {
    let repo = std::env::var("BASEMIND_HARDEN_REPO")
        .map(PathBuf::from)
        .expect("BASEMIND_HARDEN_REPO must point at a cloned repository");
    assert!(repo.join(".git").exists(), "{} is not a git clone", repo.display());
    // Every index this mode writes lands under the data home. Unset, that is the user's own cache.
    assert!(
        std::env::var_os(DATA_HOME_ENV).is_some() && std::env::var_os("BASEMIND_COMMS_DIR").is_some(),
        "set {DATA_HOME_ENV} and BASEMIND_COMMS_DIR to a throwaway directory (scripts/harden.sh does)"
    );
    let repo_name = std::env::var("BASEMIND_HARDEN_REPO_NAME").unwrap_or_else(|_| {
        repo.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string()
    });
    let count = env_count("BASEMIND_HARDEN_WORKTREES", DEFAULT_WORKTREES);
    let step = env_count("BASEMIND_HARDEN_WORKTREE_STEP", DEFAULT_COMMIT_STEP);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");

    let commits = older_commits(&repo, count, step);
    assert!(
        !commits.is_empty(),
        "{repo_name}: the clone has no commit older than HEAD to add a worktree at"
    );
    let head = git(&repo, &["rev-parse", "HEAD"]).unwrap_or_default();
    let mut rows = vec![measure_worktree(&repo, "main", head.trim(), 0)];

    let worktrees_dir = repo
        .parent()
        .expect("the clone has a parent directory")
        .join("worktrees")
        .join(&repo_name);
    let _ = git(&repo, &["worktree", "prune"]);
    let mut roots: Vec<PathBuf> = Vec::new();
    for (at, commit) in commits.iter().enumerate() {
        let root = worktrees_dir.join(format!("wt-{}", at + 1));
        let root_arg = root.to_str().expect("utf-8 worktree path");
        let _ = git(&repo, &["worktree", "remove", "--force", root_arg]);
        let _ = std::fs::remove_dir_all(&root);
        git(&repo, &["worktree", "add", "--detach", root_arg, commit])
            .unwrap_or_else(|| panic!("{repo_name}: git worktree add at {commit} failed"));
        let diff_files = git(&repo, &["diff", "--name-only", "HEAD", commit]).map_or(0, |out| out.lines().count());
        rows.push(measure_worktree(&root, &format!("wt-{}", at + 1), commit, diff_files));
        roots.push(root);
    }

    let table = render(&repo_name, &rows);
    eprintln!("\n{table}");
    if let Ok(results) = std::env::var("BASEMIND_HARDEN_RESULTS") {
        let results = Path::new(&results);
        append(&results.with_file_name("worktrees.md"), &table);
        if let Ok(line) = serde_json::to_string(&serde_json::json!({
            "repo_name": repo_name,
            "commit_step": step,
            "rows": rows,
        })) {
            append(&results.with_file_name("worktrees.ndjson"), &format!("{line}\n"));
        }
    }

    let mut cache_dirs: Vec<PathBuf> = std::iter::once(&repo)
        .chain(&roots)
        .map(|root| workspace_cache_dir(root))
        .collect();
    if std::env::var_os("BASEMIND_HARDEN_KEEP").is_none() {
        for root in &roots {
            let _ = git(
                &repo,
                &["worktree", "remove", "--force", root.to_str().unwrap_or_default()],
            );
        }
    }

    let mut failures: Vec<String> = Vec::new();
    cache_dirs.sort();
    cache_dirs.dedup();
    if cache_dirs.len() != rows.len() {
        failures.push(format!(
            "{} worktrees share {} workspace directories",
            rows.len(),
            cache_dirs.len()
        ));
    }
    for row in &rows {
        if row.scanned == 0 {
            failures.push(format!("{}: the scan saw no file", row.label));
        }
        if row.disk.manifest == 0 {
            failures.push(format!("{}: no file map on disk after the scan", row.label));
        }
        // The first worktree creates the group index; an added one may only add rows to pages fjall
        // already allocated, so only the first is held to growing it. ~keep
        if row.disk.journal + row.disk.keyspaces == 0 && row.label == "main" {
            failures.push(format!("{}: no index bytes on disk after the scan", row.label));
        }
        if row.scan_ms <= 0.0 {
            failures.push(format!("{}: the scan took no measurable time", row.label));
        }
    }
    assert!(
        failures.is_empty(),
        "[harden] {repo_name} worktree measurement did not measure:\n  - {}",
        failures.join("\n  - ")
    );
}
