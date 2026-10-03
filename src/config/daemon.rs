//! Config loading for the shared daemon.
//!
//! One process serves every workspace on the machine, and each workspace's `basemind.toml` is
//! authored by that repository, not by the operator. The resource knobs are therefore CEILINGS the
//! daemon applies on top of whatever the file says: the effective value is `min(file, cap)`, and a
//! file value that means "no limit" (`0`, `"auto"`, `"off"`) resolves to the cap. The operator raises
//! a cap through the daemon's own environment, which a repository cannot write.
//!
//! Every daemon-side load (the scan pool and the hosted read stack) goes through [`load_daemon`] so a
//! workspace never has two different configs in one process.

use std::path::Path;
use std::time::SystemTime;

use super::{Config, ConfigError, MaxFootprint, ResourcesConfig};

pub const MAX_SCAN_THREADS_ENV: &str = "BASEMIND_DAEMON_MAX_SCAN_THREADS";
pub const MAX_EMBED_THREADS_ENV: &str = "BASEMIND_DAEMON_MAX_EMBED_THREADS";
pub const MAX_EMBED_BATCH_ENV: &str = "BASEMIND_DAEMON_MAX_EMBED_BATCH";
pub const MAX_CONCURRENT_DOCUMENTS_ENV: &str = "BASEMIND_DAEMON_MAX_CONCURRENT_DOCUMENTS";
pub const MAX_FOOTPRINT_MB_ENV: &str = "BASEMIND_DAEMON_MAX_FOOTPRINT_MB";
pub const MAX_MAP_CACHE_MB_ENV: &str = "BASEMIND_DAEMON_MAX_MAP_CACHE_MB";
pub const MAX_CANDIDATES_ENV: &str = "BASEMIND_DAEMON_MAX_CANDIDATES";

pub const MAX_FILE_BYTES_ENV: &str = "BASEMIND_DAEMON_MAX_FILE_BYTES";
pub const MAX_DOC_BYTES_ENV: &str = "BASEMIND_DAEMON_MAX_DOCUMENT_BYTES";
pub const MAX_DOC_PAGES_ENV: &str = "BASEMIND_DAEMON_MAX_DOCUMENT_PAGES";
pub const MAX_EXTRACTION_SECS_ENV: &str = "BASEMIND_DAEMON_MAX_EXTRACTION_SECS";
pub const MAX_CHUNKS_PER_DOC_ENV: &str = "BASEMIND_DAEMON_MAX_CHUNKS_PER_DOCUMENT";
pub const MAX_CRAWL_PAGES_ENV: &str = "BASEMIND_DAEMON_MAX_CRAWL_PAGES";
pub const MAX_CRAWL_DEPTH_ENV: &str = "BASEMIND_DAEMON_MAX_CRAWL_DEPTH";
pub const MAX_CRAWL_BODY_BYTES_ENV: &str = "BASEMIND_DAEMON_MAX_CRAWL_BODY_BYTES";
pub const MIN_DEBOUNCE_MS_ENV: &str = "BASEMIND_DAEMON_MIN_DEBOUNCE_MS";
/// Set truthy to let a repository's `[documents] extract_archives = true` through.
pub const ALLOW_EXTRACT_ARCHIVES_ENV: &str = "BASEMIND_DAEMON_ALLOW_EXTRACT_ARCHIVES";

const DEFAULT_MAX_SCAN_THREADS: usize = 4;
const DEFAULT_MAX_EMBED_THREADS: usize = 4;
/// A bigger embed batch grows the ONNX arena, which never shrinks.
const DEFAULT_MAX_EMBED_BATCH: usize = 8;
const DEFAULT_MAX_CONCURRENT_DOCUMENTS: usize = 4;
/// Applied when a workspace leaves the footprint on auto or off; auto is half of machine RAM, which
/// on a developer laptop never engages before the machine is swapping.
pub(crate) const DEFAULT_MAX_FOOTPRINT_MB: usize = 3072;
pub(crate) const DEFAULT_MAX_MAP_CACHE_MB: usize = 1024;
pub(crate) const DEFAULT_MAX_CANDIDATES: usize = 2_000_000;

const DEFAULT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_DOC_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_DOC_PAGES: usize = 5_000;
const DEFAULT_MAX_EXTRACTION_SECS: u64 = 1_800;
const DEFAULT_MAX_CHUNKS_PER_DOC: usize = 20_000;
const DEFAULT_MAX_CRAWL_PAGES: u32 = 500;
const DEFAULT_MAX_CRAWL_DEPTH: u32 = 8;
const DEFAULT_MAX_CRAWL_BODY_BYTES: u64 = 64 * 1024 * 1024;
/// A `0` debounce makes every filesystem event trigger its own rescan.
const DEFAULT_MIN_DEBOUNCE_MS: u64 = 50;

/// Serialises every test that reads or writes the daemon-cap environment variables; `load_daemon`
/// reads them, so any test that calls it concurrently with an env-mutating one must hold this too.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A positive integer from the environment, else `default`.
fn cap(name: &str, default: usize) -> usize {
    cap_u64(name, default as u64) as usize
}

fn cap_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// Ceiling on pages per `web_crawl` call, for the MCP per-call override.
pub fn crawl_page_cap() -> u32 {
    u32::try_from(cap_u64(MAX_CRAWL_PAGES_ENV, u64::from(DEFAULT_MAX_CRAWL_PAGES))).unwrap_or(u32::MAX)
}

/// Ceiling on link depth per `web_crawl` call, for the MCP per-call override.
pub fn crawl_depth_cap() -> u32 {
    u32::try_from(cap_u64(MAX_CRAWL_DEPTH_ENV, u64::from(DEFAULT_MAX_CRAWL_DEPTH))).unwrap_or(u32::MAX)
}

/// `min(value, cap)`, with `0` (the "unbounded / auto" sentinel) resolving to `cap`.
fn bounded(value: usize, cap: usize) -> usize {
    match value {
        0 => cap,
        n => n.min(cap),
    }
}

/// Apply the daemon caps to `config` in place.
pub fn clamp_daemon_config(config: &mut Config) {
    let embed_threads = config
        .resources
        .effective_embed_threads(config.documents.embed_max_threads);
    let resources: &mut ResourcesConfig = &mut config.resources;
    resources.scan_threads = bounded(
        resources.scan_threads,
        cap(MAX_SCAN_THREADS_ENV, DEFAULT_MAX_SCAN_THREADS),
    );
    resources.embed_threads = bounded(embed_threads, cap(MAX_EMBED_THREADS_ENV, DEFAULT_MAX_EMBED_THREADS));
    resources.embed_batch_size = resources
        .embed_batch_size
        .clamp(1, cap(MAX_EMBED_BATCH_ENV, DEFAULT_MAX_EMBED_BATCH));
    resources.max_concurrent_documents = bounded(
        resources.max_concurrent_documents,
        cap(MAX_CONCURRENT_DOCUMENTS_ENV, DEFAULT_MAX_CONCURRENT_DOCUMENTS),
    );
    resources.max_footprint_mb = MaxFootprint::Mebibytes(match resources.max_footprint_mb {
        MaxFootprint::Mebibytes(mb) if mb > 0 => mb.min(cap(MAX_FOOTPRINT_MB_ENV, DEFAULT_MAX_FOOTPRINT_MB)),
        _ => cap(MAX_FOOTPRINT_MB_ENV, DEFAULT_MAX_FOOTPRINT_MB),
    });
    resources.max_map_cache_mb = bounded(
        resources.max_map_cache_mb,
        cap(MAX_MAP_CACHE_MB_ENV, DEFAULT_MAX_MAP_CACHE_MB),
    );
    config.scan.max_candidates = bounded(
        config.scan.max_candidates,
        cap(MAX_CANDIDATES_ENV, DEFAULT_MAX_CANDIDATES),
    );
    config.scan.max_file_bytes = config
        .scan
        .max_file_bytes
        .min(cap_u64(MAX_FILE_BYTES_ENV, DEFAULT_MAX_FILE_BYTES));

    let docs = &mut config.documents;
    docs.max_file_bytes = docs
        .max_file_bytes
        .min(cap_u64(MAX_DOC_BYTES_ENV, DEFAULT_MAX_DOC_BYTES));
    docs.max_pages = docs.max_pages.min(cap(MAX_DOC_PAGES_ENV, DEFAULT_MAX_DOC_PAGES));
    docs.extraction_timeout_secs = docs
        .extraction_timeout_secs
        .min(cap_u64(MAX_EXTRACTION_SECS_ENV, DEFAULT_MAX_EXTRACTION_SECS));
    docs.max_chunks_per_document = docs
        .max_chunks_per_document
        .min(cap(MAX_CHUNKS_PER_DOC_ENV, DEFAULT_MAX_CHUNKS_PER_DOC));
    if docs.extract_archives && !env_truthy(ALLOW_EXTRACT_ARCHIVES_ENV) {
        docs.extract_archives = false;
        super::trust::warn_once(format!(
            "ignoring [documents] extract_archives = true from basemind.toml: archive expansion is operator-only \
             in the daemon; set {ALLOW_EXTRACT_ARCHIVES_ENV}=1 in its environment to allow it"
        ));
    }

    let crawl = &mut config.crawl;
    crawl.max_pages = crawl.max_pages.min(crawl_page_cap());
    crawl.max_depth = crawl.max_depth.min(crawl_depth_cap());
    crawl.max_body_size = crawl
        .max_body_size
        .min(cap_u64(MAX_CRAWL_BODY_BYTES_ENV, DEFAULT_MAX_CRAWL_BODY_BYTES));

    config.watch.debounce_ms = config
        .watch
        .debounce_ms
        .max(cap_u64(MIN_DEBOUNCE_MS_ENV, DEFAULT_MIN_DEBOUNCE_MS));
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Resolve a workspace's config for the daemon, mirroring the CLI's `load_or_default`: a missing
/// `basemind.toml` falls back to defaults; only a genuine parse / IO / validation error propagates.
/// The daemon caps are applied either way.
pub fn load_daemon(root: &Path) -> Result<Config, ConfigError> {
    let mut config = match super::load_with_overrides(root, None, None) {
        Ok(loaded) => loaded.config,
        Err(ConfigError::NotFound(_)) => super::default_for_root(root),
        Err(error) => return Err(error),
    };
    clamp_daemon_config(&mut config);
    Ok(config)
}

/// Cheap change detector for a workspace's config file: modification time and length, plus a
/// content hash while the modification time is too recent to trust.
///
/// An equal-length rewrite inside one timestamp tick (coarse-mtime filesystems, or an editor and a
/// request landing in the same tick) is invisible to `(mtime, len)`. The same trick git uses for
/// "racily clean" index entries closes it: while the file was modified within [`RACY_WINDOW`] of
/// now, the stamp also carries a hash of its bytes, so two different contents never compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigStamp {
    modified: Option<SystemTime>,
    len: Option<u64>,
    content: Option<u64>,
}

/// How recent a modification time must be for the stamp to fall back to hashing the content.
const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

impl ConfigStamp {
    /// Stamp of the file [`super::resolve_config_path`] selects for `root`; a missing file stamps as
    /// the default (all `None`), so creating or deleting the file reads as a change.
    pub fn of(root: &Path) -> Self {
        let path = super::resolve_config_path(root);
        match std::fs::metadata(&path) {
            Ok(meta) => {
                let modified = meta.modified().ok();
                let racy = modified.is_none_or(|m| {
                    SystemTime::now()
                        .duration_since(m)
                        .map_or(true, |age| age < RACY_WINDOW)
                });
                Self {
                    modified,
                    len: Some(meta.len()),
                    content: racy.then(|| content_hash(&path)).flatten(),
                }
            }
            Err(_) => Self::default(),
        }
    }
}

fn content_hash(path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

impl std::fmt::Display for ConfigStamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(len) = self.len else {
            return f.write_str("absent");
        };
        let secs = self
            .modified
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        write!(f, "{len}B@{secs}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAEMON_ENVS: &[&str] = &[
        MAX_SCAN_THREADS_ENV,
        MAX_EMBED_THREADS_ENV,
        MAX_EMBED_BATCH_ENV,
        MAX_CONCURRENT_DOCUMENTS_ENV,
        MAX_FOOTPRINT_MB_ENV,
        MAX_MAP_CACHE_MB_ENV,
        MAX_CANDIDATES_ENV,
        MAX_FILE_BYTES_ENV,
        MAX_DOC_BYTES_ENV,
        MAX_DOC_PAGES_ENV,
        MAX_EXTRACTION_SECS_ENV,
        MAX_CHUNKS_PER_DOC_ENV,
        MAX_CRAWL_PAGES_ENV,
        MAX_CRAWL_DEPTH_ENV,
        MAX_CRAWL_BODY_BYTES_ENV,
        MIN_DEBOUNCE_MS_ENV,
        ALLOW_EXTRACT_ARCHIVES_ENV,
    ];

    use super::TEST_ENV_LOCK as ENV_LOCK;

    fn clear_envs() {
        for name in DAEMON_ENVS {
            // SAFETY: every test touching these variables holds ENV_LOCK.
            unsafe { std::env::remove_var(name) };
        }
    }

    fn write_toml(dir: &Path, body: &str) {
        std::fs::write(dir.join("basemind.toml"), format!("\"$schema\" = \"v1\"\n{body}")).expect("write toml");
    }

    #[test]
    fn unbounded_repo_values_resolve_to_the_caps_through_the_real_load_path() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_envs();
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(
            dir.path(),
            "[scan]\nmax_candidates = 0\n[resources]\nmax_footprint_mb = \"off\"\nmax_map_cache_mb = 0\n\
             embed_threads = 0\nmax_concurrent_documents = 0\nscan_threads = 0\n",
        );
        let cfg = load_daemon(dir.path()).expect("load");
        assert_eq!(
            cfg.resources.max_footprint_mb,
            MaxFootprint::Mebibytes(DEFAULT_MAX_FOOTPRINT_MB)
        );
        assert_eq!(cfg.resources.max_map_cache_mb, DEFAULT_MAX_MAP_CACHE_MB);
        assert_eq!(cfg.scan.max_candidates, DEFAULT_MAX_CANDIDATES);
        assert_eq!(cfg.resources.embed_threads, DEFAULT_MAX_EMBED_THREADS);
        assert_eq!(cfg.resources.max_concurrent_documents, DEFAULT_MAX_CONCURRENT_DOCUMENTS);
        assert_eq!(cfg.resources.scan_threads, DEFAULT_MAX_SCAN_THREADS);
    }

    #[test]
    fn repo_document_scan_crawl_and_watch_knobs_are_clamped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_envs();
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(
            dir.path(),
            "[scan]\nmax_file_bytes = 9223372036854775807\n\
             [documents]\nmax_file_bytes = 9223372036854775807\nmax_pages = 4000000000\n\
             extraction_timeout_secs = 999999\nmax_chunks_per_document = 4000000000\nextract_archives = true\n\
             [crawl]\nmax_pages = 4000000000\nmax_depth = 4000000000\nmax_body_size = 9223372036854775807\n\
             [watch]\ndebounce_ms = 0\n",
        );
        let cfg = load_daemon(dir.path()).expect("load");
        assert_eq!(cfg.scan.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
        assert_eq!(cfg.documents.max_file_bytes, DEFAULT_MAX_DOC_BYTES);
        assert_eq!(cfg.documents.max_pages, DEFAULT_MAX_DOC_PAGES);
        assert_eq!(cfg.documents.extraction_timeout_secs, DEFAULT_MAX_EXTRACTION_SECS);
        assert_eq!(cfg.documents.max_chunks_per_document, DEFAULT_MAX_CHUNKS_PER_DOC);
        assert!(!cfg.documents.extract_archives);
        assert_eq!(cfg.crawl.max_pages, DEFAULT_MAX_CRAWL_PAGES);
        assert_eq!(cfg.crawl.max_depth, DEFAULT_MAX_CRAWL_DEPTH);
        assert_eq!(cfg.crawl.max_body_size, DEFAULT_MAX_CRAWL_BODY_BYTES);
        assert_eq!(cfg.watch.debounce_ms, DEFAULT_MIN_DEBOUNCE_MS);
    }

    #[test]
    fn small_repo_values_and_the_archive_grant_are_kept() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_envs();
        // SAFETY: ENV_LOCK is held; cleared again below.
        unsafe { std::env::set_var(ALLOW_EXTRACT_ARCHIVES_ENV, "1") };
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(
            dir.path(),
            "[scan]\nmax_file_bytes = 4096\n[documents]\nextract_archives = true\nmax_pages = 7\n",
        );
        let cfg = load_daemon(dir.path()).expect("load");
        clear_envs();
        assert_eq!(cfg.scan.max_file_bytes, 4096);
        assert_eq!(cfg.documents.max_pages, 7);
        assert!(cfg.documents.extract_archives);
    }

    #[test]
    fn auto_and_huge_footprints_are_bounded_and_small_ones_kept() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_envs();
        let dir = tempfile::tempdir().expect("tempdir");
        for (body, want) in [
            ("max_footprint_mb = \"auto\"", DEFAULT_MAX_FOOTPRINT_MB),
            ("max_footprint_mb = 999999", DEFAULT_MAX_FOOTPRINT_MB),
            ("max_footprint_mb = 512", 512),
        ] {
            write_toml(dir.path(), &format!("[resources]\n{body}\n"));
            let cfg = load_daemon(dir.path()).expect("load");
            assert_eq!(cfg.resources.max_footprint_mb, MaxFootprint::Mebibytes(want), "{body}");
        }
    }

    #[test]
    fn operator_env_raises_the_caps() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_envs();
        // SAFETY: ENV_LOCK is held; cleared again below.
        unsafe {
            std::env::set_var(MAX_SCAN_THREADS_ENV, "12");
            std::env::set_var(MAX_FOOTPRINT_MB_ENV, "8192");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(
            dir.path(),
            "[resources]\nscan_threads = 10\nmax_footprint_mb = \"off\"\n",
        );
        let cfg = load_daemon(dir.path()).expect("load");
        clear_envs();
        assert_eq!(cfg.resources.scan_threads, 10);
        assert_eq!(cfg.resources.max_footprint_mb, MaxFootprint::Mebibytes(8192));
    }

    #[test]
    fn missing_config_still_gets_the_caps() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_envs();
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = load_daemon(dir.path()).expect("load");
        assert_eq!(cfg.resources.scan_threads, DEFAULT_MAX_SCAN_THREADS);
    }

    #[test]
    fn stamp_changes_when_the_file_is_rewritten_or_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(ConfigStamp::of(dir.path()).to_string(), "absent");
        write_toml(dir.path(), "");
        let first = ConfigStamp::of(dir.path());
        write_toml(dir.path(), "[watch]\ndebounce_ms = 500\n");
        assert_ne!(first, ConfigStamp::of(dir.path()));
    }

    #[test]
    fn an_equal_length_rewrite_with_an_identical_mtime_is_still_a_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(dir.path(), "[watch]\ndebounce_ms = 111\n");
        let path = crate::config::resolve_config_path(dir.path());
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let first = ConfigStamp::of(dir.path());

        write_toml(dir.path(), "[watch]\ndebounce_ms = 222\n");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let second = ConfigStamp::of(dir.path());

        assert_eq!(first.len, second.len, "same length");
        assert_eq!(first.modified, second.modified, "same mtime");
        assert_ne!(first, second, "the content hash tells them apart");
    }
}
