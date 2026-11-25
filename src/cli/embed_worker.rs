//! The background embedding pass a one-shot `basemind scan` hands its owed vectors to.
//!
//! A one-shot scan of the working tree writes the code map, the documents' text and chunks and the
//! keyword lane, and embeds nothing (`EmbedMode::Deferred`): it never downloads or runs the
//! embedding model. What it leaves owed is paid here, by `basemind embed-worker`, which the scan
//! starts detached ([`hand_off`]):
//!
//! - the documents the store records as owing vectors, [`BATCH_DOCUMENTS`] at a time, each batch a
//!   scoped `Inline` scan that holds the store write lock only while it runs;
//! - then, when code embedding is on (`[code_search] embed = true`), the embed policy changed, or
//!   owed documents lie beyond the batches' reach (a dependency's documentation), one whole
//!   working-tree `Inline` pass, which also settles the progress record.
//!
//! With a daemon up, the worker hands the whole pass to it instead: the daemon is the workspace's
//! writer.
//!
//! One worker runs per workspace: it holds `embed-worker.lock`, an advisory lock the OS drops with
//! the process, and a scan that finds it held starts none. The worker gives the store lock up to
//! any writer that asks (`crate::store_lock::embed_yield`), waits while a one-shot command holds
//! it, and stops when a long-lived writer (`serve`, `watch`, the daemon) takes it. What is owed
//! stays on disk (each document's entry and the record of `crate::embed_progress`), so a worker
//! that is killed is resumed by the next one, and `index_status` reports the progress meanwhile.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use fs2::FileExt;

use crate::config::Config;
use crate::scan_retrieval::Retrieval;
use crate::scanner::{EmbedMode, NullObserver, ScanCancel, ScanSource};
use crate::store::{LockHolder, Store, VIEW_WORKING, WriterProbe};

/// `false` stops `basemind scan` from starting the background embedding worker; the owed vectors
/// then wait for `basemind embed-worker`, a later scan without it, or a daemon.
pub const EMBED_BACKGROUND_ENV: &str = "BASEMIND_EMBED_BACKGROUND";

/// The advisory lock the one worker of a workspace holds, in the workspace cache dir.
const WORKER_LOCK_FILE: &str = "embed-worker.lock";

/// Where a detached worker writes its log, in the workspace cache dir.
const WORKER_LOG_FILE: &str = "embed-worker.log";

/// A log past this size is started afresh when the next worker is spawned.
const WORKER_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Owed documents a batch embeds: few enough that a writer asking for the lock waits for one
/// document, and the record advances often.
pub const BATCH_DOCUMENTS: usize = 32;

/// How long the worker sleeps before trying a store lock a one-shot command holds again.
const CONTENDED_RETRY: Duration = Duration::from_secs(1);

/// How long the worker keeps trying a store lock before it gives the pass up to whoever holds it.
const CONTENDED_GIVE_UP: Duration = Duration::from_secs(30 * 60);

/// How often the worker looks for a writer asking for the lock.
const YIELD_POLL: Duration = Duration::from_millis(100);

/// The nice value the worker runs at: it yields the CPU to the scans and queries it shares it with.
#[cfg(unix)]
const BACKGROUND_NICE: libc::c_int = 10;

/// What a scan left for the background pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    /// Documents indexed by their text that still owe their vectors.
    pub documents: usize,
    /// `[code_search] embed = true`: code chunk vectors are written by the background pass.
    pub code: bool,
    /// Vectors exist and the embed policy differs from the one the last whole embedding pass
    /// applied (or none was applied yet): the background pass rebuilds or removes vector rows.
    pub policy: bool,
}

impl Pending {
    /// What the scan of `root` that wrote `store` left owed under `config`.
    pub fn of(root: &Path, store: &Store, config: &Config) -> Self {
        let documents = crate::embed_progress::read(&crate::store::workspace_cache_dir(root))
            .map_or(0, |record| record.total.saturating_sub(record.done));
        Self {
            documents,
            code: code_embedding_on(config),
            policy: policy_changed(store, config),
        }
    }

    /// Whether the background pass has anything to do.
    pub fn any(self) -> bool {
        self.documents > 0 || self.code || self.policy
    }
}

fn code_embedding_on(config: &Config) -> bool {
    cfg!(feature = "code-search") && config.code_search.enabled && config.code_search.embed
}

/// Whether the whole pass owes the embed policy its reconciliation: the recorded policy differs
/// from the config's (none is recorded until a whole embedding pass runs) and vector rows may
/// exist for it to rebuild or remove. A repository nothing was ever embedded in owes none, so its
/// first scan starts no worker for the policy alone.
fn policy_changed(store: &Store, config: &Config) -> bool {
    store.index.embed_policy != crate::config::rules::embed_policy_digest(config)
        && (code_embedding_on(config) || documents_embedded(store))
}

/// Whether any document was embedded: it holds vectors a policy change may have to remove.
fn documents_embedded(store: &Store) -> bool {
    #[cfg(feature = "documents")]
    return store
        .index
        .doc_files
        .values()
        .any(|entry| entry.embedded && entry.embed_attempted);
    #[cfg(not(feature = "documents"))]
    {
        let _ = store;
        false
    }
}

/// What became of the pending work when the scan handed it off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    /// Nothing was owed.
    NotNeeded,
    /// A worker was started for it.
    Started,
    /// A worker already runs for this workspace and picks it up.
    AlreadyRunning,
    /// [`EMBED_BACKGROUND_ENV`] is false.
    Disabled,
    /// The worker could not be started (the log says why).
    Unavailable,
}

impl Background {
    /// The name `scan --json` reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotNeeded => "not_needed",
            Self::Started => "started",
            Self::AlreadyRunning => "already_running",
            Self::Disabled => "disabled",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Hand `pending` to a background worker for `root`, starting one unless one runs or
/// [`EMBED_BACKGROUND_ENV`] says not to.
pub fn hand_off(root: &Path, pending: Pending) -> Background {
    if !pending.any() {
        return Background::NotNeeded;
    }
    if background_disabled(std::env::var(EMBED_BACKGROUND_ENV).ok().as_deref()) {
        return Background::Disabled;
    }
    let cache_dir = crate::store::workspace_cache_dir(root);
    if worker_running(&cache_dir) {
        return Background::AlreadyRunning;
    }
    match spawn_worker(root, &cache_dir) {
        Ok(()) => Background::Started,
        Err(error) => {
            tracing::warn!(%error, "could not start the background embedding worker");
            Background::Unavailable
        }
    }
}

/// Whether a raw [`EMBED_BACKGROUND_ENV`] value turns the worker off. The binary refuses a value
/// that is not a boolean when it starts, so one that reaches here is left at the default.
fn background_disabled(value: Option<&str>) -> bool {
    crate::config::engine::bool_env(EMBED_BACKGROUND_ENV, value) == Ok(Some(false))
}

/// The summary line a scan prints for what it left owed, or `None` when it left nothing.
pub fn summary_line(pending: Pending, background: Background) -> Option<String> {
    if !pending.any() {
        return None;
    }
    let what = match (pending.documents, pending.code) {
        (0, true) => "code chunk vectors".to_string(),
        (0, false) => "vector rows of the changed embed policy".to_string(),
        (documents, true) => format!("{documents} documents and code chunk vectors"),
        (documents, false) => format!("{documents} documents"),
    };
    let how = match background {
        Background::Started | Background::NotNeeded => {
            "embedding in the background (progress: `embeddings` in `basemind status`)"
        }
        Background::AlreadyRunning => "a background embedding pass is already running and takes them",
        Background::Disabled => "background embedding is off (BASEMIND_EMBED_BACKGROUND); run `basemind embed-worker`",
        Background::Unavailable => "the background pass did not start; run `basemind embed-worker`",
    };
    Some(format!("embeddings: pending {what}, {how}"))
}

/// The `embeddings` object `scan --json` reports.
pub fn json(pending: Pending, background: Background) -> serde_json::Value {
    serde_json::json!({
        "pending_documents": pending.documents,
        "pending_code": pending.code,
        "pending_policy": pending.policy,
        "background": background.as_str(),
    })
}

/// The worker lock of a workspace cache dir, held for the worker's life.
struct WorkerLock(File);

impl WorkerLock {
    /// Take the lock, or `None` when another worker holds it.
    fn take(cache_dir: &Path) -> anyhow::Result<Option<Self>> {
        let file = open_worker_lock(cache_dir)?;
        Ok(file.try_lock_exclusive().is_ok().then_some(Self(file)))
    }
}

impl Drop for WorkerLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn open_worker_lock(cache_dir: &Path) -> anyhow::Result<File> {
    std::fs::create_dir_all(cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;
    let path = cache_dir.join(WORKER_LOCK_FILE);
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))
}

/// Whether a worker holds the lock of `cache_dir`. A lock that cannot be opened is no worker.
fn worker_running(cache_dir: &Path) -> bool {
    let Ok(file) = open_worker_lock(cache_dir) else {
        return false;
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Start `basemind embed-worker` for `root`, detached so it outlives the scan, from the cache dir
/// (never from the repository or the caller's cwd), its output appended to the worker log.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn spawn_worker(root: &Path, cache_dir: &Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--root")
        .arg(root)
        .arg("embed-worker")
        .current_dir(cache_dir)
        .stdin(std::process::Stdio::null())
        .env(
            crate::store::DATA_HOME_ENV,
            crate::data_root::DataRoot::current().path(),
        );
    match open_log(cache_dir) {
        Some((out, err)) => command.stdout(out).stderr(err),
        None => command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    };
    crate::comms::singleton::spawn_detached_process(command)
}

/// A build without the detached spawn cannot start the worker; the summary names the command.
#[cfg(not(all(feature = "comms", any(unix, windows))))]
fn spawn_worker(_root: &Path, _cache_dir: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this build cannot start a detached process",
    ))
}

#[cfg(all(feature = "comms", any(unix, windows)))]
fn open_log(cache_dir: &Path) -> Option<(File, File)> {
    let path = cache_dir.join(WORKER_LOG_FILE);
    let oversized = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > WORKER_LOG_MAX_BYTES);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(true)
        .truncate(oversized)
        .open(&path)
        .ok()?;
    let dup = log.try_clone().ok()?;
    Some((log, dup))
}

/// What a worker run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerReport {
    /// Owed documents the run settled (embedded, or found unembeddable).
    pub documents_settled: usize,
    /// Documents still owing vectors when the run ended.
    pub documents_owed: usize,
    /// Whether the run ended with the whole working-tree `Inline` pass.
    pub whole_pass: bool,
    /// Whether a long-lived writer took the store lock, so the run left the rest to it.
    pub stopped: bool,
    /// Whether the run handed the pass to the daemon.
    pub daemon: bool,
}

/// `basemind embed-worker`: pay what the scans of `root` left owed. `None` when another worker of
/// the workspace runs.
pub fn run(root: &Path, config: &Config) -> anyhow::Result<Option<WorkerReport>> {
    let cache_dir = crate::store::workspace_cache_dir(root);
    let Some(_worker) = WorkerLock::take(&cache_dir)? else {
        tracing::info!(root = %root.display(), "another embedding worker runs for this workspace");
        return Ok(None);
    };
    lower_priority();
    #[cfg(feature = "intelligence")]
    let _pass = crate::embeddings::begin_embed_pass();
    if super::rescan::daemon_is_up() {
        return via_daemon(root).map(Some);
    }
    run_local(root, config, crate::retrieval_wiring::backend()).map(Some)
}

/// Forward the whole pass to the daemon, the workspace's writer: one working-tree rescan that
/// embeds, which the daemon schedules beside its other work.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn via_daemon(root: &Path) -> anyhow::Result<WorkerReport> {
    super::rescan::rescan_via_daemon(root, None, true).context("forward the embedding pass to the daemon")?;
    Ok(WorkerReport {
        whole_pass: true,
        daemon: true,
        ..WorkerReport::default()
    })
}

#[cfg(not(all(feature = "comms", any(unix, windows))))]
fn via_daemon(_root: &Path) -> anyhow::Result<WorkerReport> {
    anyhow::bail!("no daemon in this build")
}

/// The embedding pass in this process, with `retrieval`'s embedder. The worker's body, and the
/// seam a test drives with a stand-in embedder.
pub fn run_local(root: &Path, config: &Config, retrieval: Retrieval) -> anyhow::Result<WorkerReport> {
    let asks = Asks::of(root);
    let mut report = WorkerReport::default();
    #[cfg(feature = "documents")]
    if !embed_owed_documents(root, config, retrieval, &asks, &mut report)? {
        report.stopped = true;
        return Ok(report);
    }
    let Some(mut store) = open_writer(root, &asks)? else {
        report.stopped = true;
        return Ok(report);
    };
    let owed_left = owed_documents(&store);
    if !(code_embedding_on(config) || policy_changed(&store, config) || owed_left > 0) {
        crate::embed_progress::clear(&crate::store::workspace_cache_dir(root));
        return Ok(report);
    }
    report.whole_pass = true;
    loop {
        let cancel = ScanCancel::new();
        let yielded = {
            let _poll = YieldPoll::start(asks.clone(), cancel.clone());
            crate::scanner::scan_with_observer(
                root,
                &mut store,
                config,
                ScanSource::WorkingTree,
                EmbedMode::Inline,
                &cancel,
                &mut NullObserver,
                retrieval,
            )
            .context("the background working-tree embedding pass")?
            .cancelled
        };
        if !yielded {
            break;
        }
        drop(store);
        store = match open_writer(root, &asks)? {
            Some(store) => store,
            None => {
                report.stopped = true;
                return Ok(report);
            }
        };
    }
    report.documents_owed = owed_documents(&store);
    Ok(report)
}

/// Embed the owed documents batch by batch. `false` when a long-lived writer took the lock.
#[cfg(feature = "documents")]
fn embed_owed_documents(
    root: &Path,
    config: &Config,
    retrieval: Retrieval,
    asks: &Asks,
    report: &mut WorkerReport,
) -> anyhow::Result<bool> {
    let settled = std::sync::atomic::AtomicUsize::new(0);
    let mut reporter = None;
    loop {
        let Some(mut store) = open_writer(root, asks)? else {
            return Ok(false);
        };
        let owed = reachable_owed_documents(root, &store);
        let reporter = reporter.get_or_insert_with(|| {
            crate::embed_progress::EmbedReporter::begin(crate::store::workspace_cache_dir(root), owed.len(), &settled)
        });
        if owed.is_empty() {
            return Ok(true);
        }
        let batch = &owed[..owed.len().min(BATCH_DOCUMENTS)];
        let cancel = ScanCancel::new();
        let yielded = {
            let _poll = YieldPoll::start(asks.clone(), cancel.clone());
            crate::scanner::scan_paths_with_observer(
                root,
                &mut store,
                config,
                batch,
                EmbedMode::Inline,
                &cancel,
                &mut NullObserver,
                retrieval,
            )
            .context("a background document embedding batch")?
            .cancelled
        };
        let still_owed: std::collections::HashSet<PathBuf> =
            reachable_owed_documents(root, &store).into_iter().collect();
        drop(store);
        let paid = batch.iter().filter(|path| !still_owed.contains(*path)).count();
        report.documents_settled += paid;
        settled.store(report.documents_settled, Ordering::Relaxed);
        reporter.report();
        // A batch that settled nothing without being asked to stop cannot be paid by another one:
        // the whole pass after the batches gets what is left.
        if paid == 0 && !yielded {
            return Ok(true);
        }
    }
}

/// The owed documents a scoped scan can reach: in the working tree and still on disk, in path order.
#[cfg(feature = "documents")]
fn reachable_owed_documents(root: &Path, store: &Store) -> Vec<PathBuf> {
    crate::embed_progress::owed_doc_paths(store)
        .into_iter()
        .filter(|rel| !rel.is_external())
        .map(|rel| root.join(rel.to_path_buf()))
        .filter(|abs| abs.is_file())
        .collect()
}

fn owed_documents(store: &Store) -> usize {
    #[cfg(feature = "documents")]
    return crate::embed_progress::owed_doc_paths(store).len();
    #[cfg(not(feature = "documents"))]
    {
        let _ = store;
        0
    }
}

/// Open the working view for the worker. Waits while a writer asks for the lock or a one-shot
/// command holds it; `None` once a long-lived writer holds it, or after [`CONTENDED_GIVE_UP`].
fn open_writer(root: &Path, asks: &Asks) -> anyhow::Result<Option<Store>> {
    let started = Instant::now();
    loop {
        asks.wait_while_asked();
        match Store::open_with_holder(root, VIEW_WORKING, LockHolder::EmbedWorker) {
            Ok(store) => return Ok(Some(store)),
            Err(error) if error.is_lock_contention() => {
                if held_by_long_lived_writer(&asks.workspace) || started.elapsed() >= CONTENDED_GIVE_UP {
                    tracing::info!(%error, "the store is held by another writer; leaving the embedding to it");
                    return Ok(None);
                }
                std::thread::sleep(CONTENDED_RETRY);
            }
            Err(error) => return Err(anyhow::Error::new(error).context("open the store for the embedding worker")),
        }
    }
}

/// Whether the store lock of `basemind_dir` is held by a writer that keeps the index fresh itself.
fn held_by_long_lived_writer(basemind_dir: &Path) -> bool {
    let WriterProbe::Held { holder: Some(meta) } = crate::store::probe_writer_lock(basemind_dir) else {
        return false;
    };
    [LockHolder::Serve, LockHolder::Watch, LockHolder::Daemon]
        .iter()
        .any(|holder| holder.command() == meta.command)
}

/// The two locks a store open takes, each of which a writer may ask the worker to give up: the
/// workspace's and its repository group's.
#[derive(Clone, Debug)]
struct Asks {
    workspace: PathBuf,
    group: PathBuf,
}

impl Asks {
    fn of(root: &Path) -> Self {
        Self {
            workspace: crate::store::workspace_cache_dir(root),
            group: crate::store_layout::group_index_dir(root),
        }
    }

    fn asked(&self) -> bool {
        crate::store_lock::embed_yield::requested(&self.workspace)
            || crate::store_lock::embed_yield::requested(&self.group)
    }

    fn wait_while_asked(&self) {
        while self.asked() {
            std::thread::sleep(YIELD_POLL);
        }
    }
}

/// Trips a pass's cancel token once a writer asks for the lock, so the pass stops at the next file
/// boundary with what it completed committed. Stops polling when dropped.
struct YieldPoll {
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl YieldPoll {
    fn start(asks: Asks, cancel: ScanCancel) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let watching = Arc::clone(&done);
        let thread = std::thread::Builder::new()
            .name("bm-embed-yield".to_string())
            .spawn(move || {
                while !watching.load(Ordering::Relaxed) {
                    if asks.asked() {
                        cancel.cancel();
                        return;
                    }
                    std::thread::sleep(YIELD_POLL);
                }
            })
            .map_err(
                |error| tracing::warn!(%error, "no yield poll: the pass runs to its end before a writer gets the lock"),
            )
            .ok();
        Self { done, thread }
    }
}

impl Drop for YieldPoll {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Run the worker at a lower CPU priority than the scans and queries beside it.
#[cfg(unix)]
fn lower_priority() {
    // SAFETY: `setpriority` takes only integers and changes this process's own nice value.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, BACKGROUND_NICE) };
    if rc != 0 {
        tracing::debug!(error = %std::io::Error::last_os_error(), "could not lower the embedding worker's priority");
    }
}

#[cfg(not(unix))]
fn lower_priority() {}

#[cfg(test)]
#[path = "embed_worker_tests.rs"]
mod tests;
