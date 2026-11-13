//! Build a committed, code-only base generation without changing existing workspace reads.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::git::Repo;
use crate::index::sorted_run::SortedRunView;
use crate::index::view::IndexView;
use crate::index::{INDEX_DIR, INDEX_SCHEMA_VER};
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, repo_group_dir};
use crate::store_base_retention::{GenerationLease, highest_leased_generation};

const BASE_DIR: &str = "base";
const BASE_LOCK_FILE: &str = "base.lock";
const RUNS: &str = "runs";
const MANIFEST: &str = "base.json";
const FORMAT: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct BaseManifest {
    format: u32,
    storage: Storage,
    generation: u64,
    commit: String,
    tree: String,
    source_ref: String,
    built_unix: u64,
    basemind_version: String,
    index_schema_ver: u32,
    blob_schema_ver: u16,
    min_extract_epoch: u16,
    scan_features: String,
    scan_digest: String,
    embed_policy: String,
    files: usize,
    bytes: u64,
    probes_complete: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Storage {
    Fjall,
    SortedRun,
}

#[derive(Debug)]
pub enum BaseBuild {
    Ready(PathBuf),
    Busy,
    Ineligible,
}

/// Build or reuse a committed base for the repository root and scan policy.
///
/// This is an explicit preparation step. It leaves the workspace view unchanged
/// until overlay creation and equivalence verification are implemented.
pub fn prepare(root: &Path, config: &Config) -> Result<BaseBuild> {
    let repo = match Repo::discover(root) {
        Ok(repo) => repo,
        Err(crate::git::GitError::NotARepo(_)) => return Ok(BaseBuild::Ineligible),
        Err(error) => return Err(error.into()),
    };
    if !repo.origin_is_workdir() || config.scan.follow_symlinks {
        return Ok(BaseBuild::Ineligible);
    }

    let group = repo_group_dir(root);
    let base = group.join(BASE_DIR);
    fs::create_dir_all(&base).context("create base generation directory")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(group.join(BASE_LOCK_FILE))
        .context("open base generation lock")?;
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&lock) {
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(BaseBuild::Busy);
        }
        return Err(error).context("lock base generation directory");
    }

    let (source_ref, commit) = resolve_commit(&repo)?;
    let digest = scan_digest(config)?;
    let (highest, ready) = find_generation(&base, &commit, &digest)?;
    if let Some(path) = ready {
        return Ok(BaseBuild::Ready(path));
    }
    if crate::disk_space::ensure_free_space(&base).is_err() {
        return Ok(BaseBuild::Ineligible);
    }
    let generation = highest
        .max(highest_leased_generation(&group)?)
        .checked_add(1)
        .context("base generation counter exhausted")?;
    let tree = repo.resolve_rev(&format!("{commit}^{{tree}}"))?;
    let metadata = BaseManifest {
        format: FORMAT,
        storage: Storage::SortedRun,
        generation,
        commit,
        tree,
        source_ref,
        built_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        basemind_version: env!("CARGO_PKG_VERSION").into(),
        index_schema_ver: INDEX_SCHEMA_VER,
        blob_schema_ver: crate::extract::SCHEMA_VER,
        min_extract_epoch: crate::extract::EXTRACT_EPOCH,
        scan_features: scan_features(),
        scan_digest: digest,
        embed_policy: String::new(),
        files: 0,
        bytes: 0,
        probes_complete: false,
    };
    build_generation(&base, &repo, config, metadata).map(BaseBuild::Ready)
}

/// Open a published base for the exact commit and scan policy supplied by its caller.
pub fn open(root: &Path, config: &Config, generation: u64, commit: &str) -> Result<SortedRunView> {
    let group = repo_group_dir(root);
    let lease = std::sync::Arc::new(GenerationLease::open(&group, generation)?);
    ensure!(
        is_compatible(lease.path(), generation, commit, &scan_digest(config)?),
        "incompatible base generation"
    );
    Ok(SortedRunView::open_leased(lease, RUNS)?)
}

/// Open an opt-in committed index for a clean worktree. Callers can use their ordinary
/// working index whenever the worktree or scan policy requires an overlay.
pub fn open_clean_worktree(root: &Path, config: &Config) -> Result<Option<SortedRunView>> {
    open_worktree_base(root, config, true)
}

pub(crate) fn open_existing_worktree_base(root: &Path, config: &Config) -> Result<Option<SortedRunView>> {
    open_worktree_base(root, config, false)
}

fn eligible(config: &Config) -> bool {
    !(config.documents.enabled
        || config.documents.embed
        || config.code_search.enabled
        || config.code_search.embed
        || config.scan.follow_symlinks
        || !config.scan.extra_roots.is_empty()
        || !config.scan.respect_gitignore
        || !config.scan.skip_submodules)
}

fn open_worktree_base(root: &Path, config: &Config, build: bool) -> Result<Option<SortedRunView>> {
    if !eligible(config) {
        return Ok(None);
    }
    let repo = match Repo::discover(root) {
        Ok(repo) if repo.origin_is_workdir() => repo,
        Ok(_) | Err(crate::git::GitError::NotARepo(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let (_, commit) = resolve_commit(&repo)?;
    if !repo.status_against(&commit)?.overlay_candidates().is_empty() {
        return Ok(None);
    }
    let path = if build {
        let BaseBuild::Ready(path) = prepare(root, config)? else {
            return Ok(None);
        };
        path
    } else {
        let directory = repo_group_dir(root).join(BASE_DIR);
        if !directory.is_dir() {
            return Ok(None);
        }
        let (_, ready) = find_generation(&directory, &commit, &scan_digest(config)?)?;
        let Some(path) = ready else {
            return Ok(None);
        };
        path
    };
    let generation = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("gen-"))
        .and_then(|id| id.parse().ok())
        .context("invalid prepared generation name")?;
    let view = open(root, config, generation, &commit)?;
    if !repo.status_against(&commit)?.overlay_candidates().is_empty() {
        return Ok(None);
    }
    Ok(Some(view))
}

pub fn publish_dirty_worktree(root: &Path, config: &Config, reference: &dyn IndexView) -> Result<bool> {
    Ok(dirty_worktree(root, config, Some(reference))?.is_some())
}

/// Reuse an exact delta previously checked against a complete standalone scan.
pub fn has_verified_dirty_worktree(root: &Path, config: &Config) -> Result<bool> {
    Ok(open_dirty_worktree(root, config)?.is_some())
}

pub(crate) fn open_dirty_worktree(root: &Path, config: &Config) -> Result<Option<Box<dyn IndexView>>> {
    dirty_worktree(root, config, None)
}

fn dirty_worktree(
    root: &Path,
    config: &Config,
    reference: Option<&dyn IndexView>,
) -> Result<Option<Box<dyn IndexView>>> {
    use crate::index::layered_shadow::DeltaManifest;
    use crate::index::layered_view::LayeredView;

    if !eligible(config) {
        return Ok(None);
    }
    let repo = Repo::discover(root)?;
    if !repo.origin_is_workdir() {
        return Ok(None);
    }
    let (_, commit) = resolve_commit(&repo)?;
    let candidates = repo.status_against(&commit)?.overlay_candidates();
    if candidates.is_empty()
        || candidates.len() > 32
        || candidates.iter().any(|path| {
            path.as_str().is_none_or(|path| {
                path.split('/').any(|component| component.starts_with('.'))
                    || ![".rs", ".ts", ".tsx", ".py"]
                        .iter()
                        .any(|extension| path.ends_with(extension))
            })
        })
    {
        return Ok(None);
    }
    let directory = repo_group_dir(root).join(BASE_DIR);
    if !directory.is_dir() {
        return Ok(None);
    }
    let (_, ready) = find_generation(&directory, &commit, &scan_digest(config)?)?;
    let Some(path) = ready else {
        return Ok(None);
    };
    let generation: u64 = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("gen-"))
        .context("invalid generation directory")?
        .parse()?;
    let base = open(root, config, generation, &commit)?;
    let base_index = crate::store::read_index(&path)?.context("missing base manifest")?;
    let fingerprint = candidate_fingerprint(root, &candidates, config.scan.max_file_bytes)?;
    let overlays = crate::store::workspace_cache_dir(root).join("verified-overlays");
    let published = overlays.join(format!("{generation}-{fingerprint}"));
    let mut counts = ahash::AHashMap::new();
    for entry in base_index.files.values() {
        *counts.entry(entry.hash_hex.as_str()).or_default() += 1;
    }
    if let Some(reference) = reference.filter(|_| !published.is_dir()) {
        fs::create_dir_all(&overlays)?;
        let scratch = tempfile::tempdir_in(&overlays)?;
        let mut overlay = Store::open_base_scratch(root, scratch.path())?;
        let paths = candidates
            .iter()
            .map(|path| root.join(path.to_path_buf()))
            .collect::<Vec<_>>();
        let report = crate::scanner::scan_paths(root, &mut overlay, config, &paths, EmbedMode::Deferred)?;
        ensure!(
            !report.cancelled && report.stats.read_failed == 0 && report.stats.extract_failed == 0,
            "incomplete delta scan"
        );
        let mut delta = DeltaManifest::empty(generation, commit.clone(), 1);
        for path in &candidates {
            let prior = base_index.files.get(path);
            if let Some(entry) = overlay.lookup(path) {
                delta.upsert(path.clone(), entry.clone(), prior);
            } else if prior.is_some() {
                delta.delete(path.clone());
            }
        }
        #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
        restitch_overlay(
            root,
            config,
            &mut overlay,
            &path,
            generation,
            &commit,
            &base_index,
            &candidates,
            &mut delta,
        )?;
        ensure!(
            repo.status_against(&commit)?.overlay_candidates() == candidates,
            "worktree changed during delta scan"
        );
        ensure!(
            candidate_fingerprint(root, &candidates, config.scan.max_file_bytes)? == fingerprint,
            "candidate contents changed during delta scan"
        );
        let shadow = delta.shadow(&counts, |path| base_index.files.get(path));
        let db = overlay.index_db.as_ref().context("missing overlay database")?;
        let verification_base = open(root, config, generation, &commit)?;
        let view = LayeredView::from_view(Box::new(verification_base), db, shadow)?;
        crate::index::verify_shared_base(&view, reference)?;
        drop(view);
        fs::write(scratch.path().join("delta.msgpack"), rmp_serde::to_vec_named(&delta)?)?;
        drop(overlay);
        sync_tree(scratch.path())?;
        let group = repo_group_dir(root);
        let gate = crate::store_base_retention::group_lock(&group)?;
        fs2::FileExt::lock_exclusive(&gate)?;
        write_overlay_pin(&group, root, generation)?;
        fs::rename(scratch.path(), &published)?;
        sync_directory(&overlays)?;
    }
    if !published.is_dir() {
        return Ok(None);
    }
    let delta: DeltaManifest = rmp_serde::from_slice(&fs::read(published.join("delta.msgpack"))?)?;
    ensure!(
        delta.format == 1 && delta.base_generation == generation && delta.base_commit == commit && delta.seq == 1,
        "overlay identity mismatch"
    );
    let shadow = delta.shadow(&counts, |path| base_index.files.get(path));
    let db = crate::index::IndexDb::open(&published)?;
    ensure!(!db.wiped_on_open(), "overlay schema mismatch");
    let view = LayeredView::from_view(Box::new(base), &db, shadow)?;
    Ok(Some(Box::new(view)))
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[allow(clippy::too_many_arguments)]
fn restitch_overlay(
    root: &Path,
    config: &Config,
    overlay: &mut Store,
    base_path: &Path,
    generation: u64,
    commit: &str,
    base_index: &crate::store::Index,
    candidates: &std::collections::BTreeSet<crate::path::RelPath>,
    delta: &mut crate::index::layered_shadow::DeltaManifest,
) -> Result<()> {
    use crate::intel::resolve_pass::{PathDelta, resolve_with_probe_view};

    let base = open(root, config, generation, commit)?;
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for path in candidates {
        let name = path.as_str().context("non-UTF-8 overlay path")?.to_owned();
        if overlay.lookup(path).is_some() {
            changed.push(name);
        } else {
            removed.push(name);
        }
    }
    for (path, entry) in &base_index.files {
        if candidates.contains(path) {
            continue;
        }
        overlay.upsert(path.clone(), entry.clone());
    }
    overlay.fallback_blobs_dir = Some(base_path.join(crate::store_layout::BLOBS_DIR));
    let db = overlay.index_db.as_ref().context("missing overlay database")?;
    let (complete, affected) = resolve_with_probe_view(
        root,
        overlay,
        &PathDelta {
            changed: &changed,
            removed: &removed,
            other: &[],
        },
        config.code_intel.precise_resolution,
        config.resources.max_footprint_mb,
        &base,
    );
    ensure!(complete, "overlay restitch incomplete");
    delta.add_restitched(affected);
    db.set_import_probes_complete(true)?;
    Ok(())
}

fn write_overlay_pin(group: &Path, root: &Path, generation: u64) -> Result<()> {
    use std::io::Write;

    let pins = group.join("overlay-pins");
    fs::create_dir_all(&pins)?;
    let name = format!("{}-{generation}", crate::store_layout::workspace_key(root));
    let mut pin = tempfile::NamedTempFile::new_in(&pins)?;
    pin.write_all(&generation.to_be_bytes())?;
    pin.as_file().sync_all()?;
    pin.persist(pins.join(name))?;
    sync_directory(&pins)?;
    Ok(())
}

fn durable_overlay_pins(group: &Path) -> io::Result<std::collections::BTreeSet<u64>> {
    let entries = match fs::read_dir(group.join("overlay-pins")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error),
    };
    entries
        .map(|entry| {
            let entry = entry?;
            let bytes: [u8; 8] = fs::read(entry.path())?
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid overlay pin"))?;
            Ok(u64::from_be_bytes(bytes))
        })
        .collect()
}

/// Retire older published base generations while keeping the newest two and every durable pin.
pub fn retire_old_generations(root: &Path) -> Result<usize> {
    let group = repo_group_dir(root);
    let base = group.join(BASE_DIR);
    if !base.is_dir() {
        return Ok(0);
    }
    let mut ready = std::collections::BTreeSet::<u64>::new();
    let mut retired = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(generation) = name.strip_prefix("gen-").and_then(|id| id.parse().ok()) {
            ready.insert(generation);
        } else if let Some(generation) = name.strip_prefix(".retired-").and_then(|id| id.parse().ok()) {
            retired.insert(generation);
        }
    }
    retired.extend(ready.iter().rev().skip(2).copied());
    let result = crate::store_base_retention::retire_unpinned(&group, &retired, || durable_overlay_pins(&group))?;
    Ok(result.removed.len())
}

fn candidate_fingerprint(
    root: &Path,
    candidates: &std::collections::BTreeSet<crate::path::RelPath>,
    max_bytes: u64,
) -> Result<String> {
    use std::io::Read;
    let mut digest = blake3::Hasher::new();
    for path in candidates {
        let name: &[u8] = path.as_ref();
        digest.update(&(name.len() as u64).to_le_bytes());
        digest.update(name);
        let path = root.join(path.to_path_buf());
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                digest.update(&[0]);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file() && metadata.len() <= max_bytes,
            "unsupported overlay candidate"
        );
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= max_bytes, "overlay candidate exceeds size limit");
        digest.update(&[1]);
        digest.update(blake3::hash(&bytes).as_bytes());
    }
    Ok(digest.finalize().to_hex().to_string())
}

fn resolve_commit(repo: &Repo) -> Result<(String, String)> {
    const ORIGIN: &str = "refs/remotes/origin/HEAD";
    match repo.resolve_rev(ORIGIN) {
        Ok(commit) => Ok((ORIGIN.into(), commit)),
        Err(_) => Ok((
            "HEAD".into(),
            Repo::discover(&repo.main_worktree_root())?.resolve_rev("HEAD")?,
        )),
    }
}

fn scan_features() -> String {
    [
        ("code-search", cfg!(feature = "code-search")),
        ("code-intel-js", cfg!(feature = "code-intel-js")),
        ("code-intel-stack", cfg!(feature = "code-intel-stack")),
    ]
    .into_iter()
    .filter_map(|(name, enabled)| enabled.then_some(name))
    .collect::<Vec<_>>()
    .join(",")
}

fn scan_digest(config: &Config) -> Result<String> {
    let mut scan = config.scan.clone();
    scan.extra_roots.clear();
    let policy = serde_json::to_vec(&(&scan, &config.languages, &config.code_intel, &config.code_search))?;
    Ok(blake3::hash(&policy).to_hex().to_string())
}

fn find_generation(base: &Path, commit: &str, digest: &str) -> Result<(u64, Option<PathBuf>)> {
    let mut highest = 0;
    let mut ready = None;
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(number) = name.strip_prefix("gen-").or_else(|| name.strip_prefix(".retired-")) else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        highest = highest.max(number);
        let path = entry.path();
        if name.starts_with("gen-")
            && entry.file_type()?.is_dir()
            && is_compatible(&path, number, commit, digest)
            && ready.as_ref().is_none_or(|(current, _)| number > *current)
        {
            ready = Some((number, path));
        }
    }
    Ok((highest, ready.map(|(_, path)| path)))
}

fn is_compatible(path: &Path, generation: u64, commit: &str, digest: &str) -> bool {
    let Some(manifest): Option<BaseManifest> = fs::read(path.join(MANIFEST))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    else {
        return false;
    };
    matches!(manifest.storage, Storage::SortedRun)
        && manifest.format == FORMAT
        && manifest.generation == generation
        && manifest.commit == commit
        && manifest.scan_digest == digest
        && manifest.index_schema_ver == INDEX_SCHEMA_VER
        && manifest.blob_schema_ver == crate::extract::SCHEMA_VER
        && manifest.min_extract_epoch >= crate::extract::EXTRACT_EPOCH
        && manifest.scan_features == scan_features()
        && (!requires_probes() || manifest.probes_complete)
        && crate::store::read_index(path)
            .ok()
            .flatten()
            .is_some_and(|index| index.files.len() == manifest.files)
        && path.join(RUNS).is_dir()
}

fn build_generation(base: &Path, repo: &Repo, config: &Config, mut metadata: BaseManifest) -> Result<PathBuf> {
    let staging = base.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    create_private_dir(&staging)?;
    let scratch = Scratch(staging);
    let checkout = scratch.0.join("checkout");
    repo.export_tree(&metadata.commit, &checkout, config.scan.max_file_bytes)?;
    let mut base_config = config.clone();
    base_config.scan.extra_roots.clear();
    base_config.documents.enabled = false;
    let mut store = Store::open_base_scratch(&checkout, &scratch.0)?;
    let report = scan(
        &checkout,
        &mut store,
        &base_config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )?;
    ensure!(
        !report.cancelled && report.stats.read_failed == 0 && report.stats.extract_failed == 0,
        "base scan did not index all eligible files"
    );
    let status = crate::store::read_status_sidecar(&scratch.0).context("base scan has no status")?;
    ensure!(!status.is_degraded(), "base scan is incomplete");
    let index = store.index_db.as_ref().context("base scan has no index")?;
    metadata.probes_complete = index.view().import_probes_complete();
    ensure!(
        !index.is_poisoned() && (!requires_probes() || metadata.probes_complete),
        "base resolve pass is incomplete"
    );
    metadata.files = store.index.files.len();
    metadata.embed_policy.clone_from(&store.index.embed_policy);
    for entry in store.index.files.values_mut() {
        entry.mtime = 0;
        metadata.bytes = metadata.bytes.saturating_add(entry.size_bytes);
    }
    let runs = scratch.0.join(RUNS);
    fs::create_dir(&runs)?;
    SortedRunView::export(&runs, &index.view())?;
    store.flush()?;
    drop(store);
    fs::remove_dir_all(scratch.0.join(INDEX_DIR))?;
    fs::remove_dir_all(checkout)?;
    fs::remove_dir_all(scratch.0.join(crate::store::MEMORY_STORE_DIR))?;
    sync_tree(&scratch.0).context("sync staged base files")?;
    let manifest = scratch.0.join(MANIFEST);
    fs::write(&manifest, serde_json::to_vec(&metadata)?)?;
    sync_file(&manifest).with_context(|| format!("sync base manifest {}", manifest.display()))?;
    sync_directory(&scratch.0)?;
    let published = base.join(format!("gen-{}", metadata.generation));
    fs::rename(&scratch.0, &published)?;
    sync_directory(base)?;
    Ok(published)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

const fn requires_probes() -> bool {
    cfg!(any(feature = "code-intel-js", feature = "code-intel-stack"))
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sync_tree(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            sync_file(&entry.path())?;
        }
    }
    sync_directory(path)
}

fn sync_file(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    let file = options.open(path)?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: The live file descriptor belongs to `file`. F_FULLFSYNC takes no extra argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    file.sync_all()
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return fs::File::open(path)?.sync_all();
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.test")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn committed_base_is_atomic_reusable_and_independent_of_working_state() {
        crate::store::init_isolated_cache();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        let file = root.join("example.rs");
        git(root, &["init", "-q"]);
        fs::write(&file, "pub fn original() {}").unwrap();
        git(root, &["add", "."]);
        git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
        fs::write(&file, "pub fn uncommitted() {}").unwrap();
        fs::write(root.join("untracked.rs"), "pub fn untracked() {}").unwrap();

        let mut config = Config::with_defaults();
        config.documents.embed = true;
        config.code_search.embed = true;
        let BaseBuild::Ready(first) = prepare(root, &config).unwrap() else {
            panic!("committed base was not published");
        };
        assert_eq!(first.file_name().unwrap(), "gen-1");
        let index = crate::store::read_index(&first).unwrap().unwrap();
        assert_eq!(index.files.len(), 1);
        let entry = index.files.get(&crate::path::RelPath::from("example.rs")).unwrap();
        assert_eq!(entry.mtime, 0);
        let expected = blake3::hash(b"pub fn original() {}");
        assert_eq!(entry.hash_hex, expected.to_hex().to_string());
        assert!(!first.join("checkout").exists());
        assert!(!first.join("lance").exists());

        let BaseBuild::Ready(reused) = prepare(root, &config).unwrap() else {
            panic!("compatible generation was not reused");
        };
        assert_eq!(reused, first);

        let commit = Repo::discover(root).unwrap().resolve_rev("HEAD").unwrap();
        assert!(open(root, &config, 1, "wrong").is_err());
        let reader = open(root, &config, 1, &commit).unwrap();
        let key = crate::index::keys::symbols_by_path_prefix(&crate::path::RelPath::from("example.rs"));
        let (_, retained) = reader
            .prefix(crate::index::view::Part::SymbolsByPath, &key)
            .next()
            .unwrap()
            .unwrap();
        assert!(!retained.is_empty());
        drop(reader);
        assert!(!first.join(INDEX_DIR).exists());
        let group = repo_group_dir(root);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(group.join(BASE_LOCK_FILE))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(matches!(prepare(root, &config).unwrap(), BaseBuild::Busy));
        drop(lock);

        assert!(open_clean_worktree(root, &config).unwrap().is_none());
        use crate::store_base_retention::retire_unpinned;
        use std::collections::BTreeSet;
        let generations = BTreeSet::from([1]);
        let retired = retire_unpinned(&group, &generations, || Ok(BTreeSet::new())).unwrap();
        assert_eq!(retired.active, [1]);
        assert!(first.is_dir());
        drop(retained);
        let retired = retire_unpinned(&group, &generations, || Ok(BTreeSet::new())).unwrap();
        assert_eq!(retired.removed, [1]);
        let BaseBuild::Ready(second) = prepare(root, &config).unwrap() else {
            panic!("invalid generation was not replaced");
        };
        assert_eq!(second.file_name().unwrap(), "gen-2");
    }
}

#[cfg(all(test, feature = "code-intel-js"))]
#[test]
fn published_overlay_restitches_unchanged_importers() {
    verify_moved_export(
        "ts",
        "export function target() {}\n",
        "import { target } from './target';\ntarget();\n",
        "// moved\nexport function target() {}\n",
    );
    verify_moved_export(
        "py",
        "def target():\n    return 1\n",
        "from target import target\ntarget()\n",
        "# moved\ndef target():\n    return 1\n",
    );
}

#[cfg(all(test, feature = "code-intel-js"))]
fn verify_moved_export(extension: &str, original: &str, importer: &str, changed: &str) {
    crate::store::init_isolated_cache();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    let target_name = format!("target.{extension}");
    let importer_name = format!("use.{extension}");
    fs::write(root.join(&target_name), original).unwrap();
    fs::write(root.join(&importer_name), importer).unwrap();
    git(&["add", &target_name, &importer_name]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
    let mut config = Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    assert!(open_clean_worktree(root, &config).unwrap().is_some());
    fs::write(root.join(&target_name), changed).unwrap();
    let mut standalone = Store::open(root, "working").unwrap();
    scan(
        root,
        &mut standalone,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .unwrap();
    let reference = standalone.index_view().unwrap();
    assert!(publish_dirty_worktree(root, &config, &reference).unwrap());
    let view = open_dirty_worktree(root, &config).unwrap().unwrap();
    let target = crate::path::RelPath::from(target_name);
    let offset = u32::try_from(changed.find("target").unwrap()).unwrap();
    let expected = reference.references_to(&target, offset);
    assert!(!expected.is_empty());
    assert_eq!(view.references_to(&target, offset), expected);
    drop(view);
    let group = repo_group_dir(root);
    let base = group.join(BASE_DIR);
    fs::create_dir(base.join("gen-2")).unwrap();
    fs::create_dir(base.join("gen-3")).unwrap();
    assert_eq!(retire_old_generations(root).unwrap(), 0);
    assert!(base.join("gen-1").is_dir());
    let pin = group
        .join("overlay-pins")
        .join(format!("{}-1", crate::store_layout::workspace_key(root)));
    fs::remove_file(pin).unwrap();
    assert_eq!(retire_old_generations(root).unwrap(), 1);
    assert!(!base.join("gen-1").exists());
}
