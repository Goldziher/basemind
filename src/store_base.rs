//! Build a committed, code-only base generation without changing existing workspace reads.

use std::fs::{self, File, OpenOptions};
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
    return File::open(path)?.sync_all();
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
