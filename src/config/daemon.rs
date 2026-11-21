//! Config loading for the shared daemon.
//!
//! One process serves every workspace on the machine, and each workspace's `basemind.toml` is
//! authored by that repository, not by the operator. The resource knobs are therefore CEILINGS the
//! daemon applies on top of whatever the file says: the effective value is `min(file, cap)`, and a
//! file value that means "no limit" (`0`, `"auto"`, `"off"`) resolves to the cap. The operator raises
//! a cap through the daemon's own environment, which a repository cannot write: the CLI reads the
//! `BASEMIND_DAEMON_MAX_*` variables into [`DaemonCaps`] ([`DaemonCaps::from_vars`]), which the
//! engine configuration carries.
//!
//! Every daemon-side load (the scan pool and the hosted read stack) goes through [`load_daemon`] so a
//! workspace never has two different configs in one process.
//!
//! How many expensive calls the server runs at once is the operator's too, and has no file value at
//! all: one admission controller serves every workspace of the daemon, so no repository's file may
//! size it. The CLI reads the `BASEMIND_DAEMON_*_CONCURRENCY` and `BASEMIND_DAEMON_QUEUE_WAIT_MS`
//! variables into [`AdmissionLimits`] ([`AdmissionLimits::from_vars`]).

use std::path::Path;
use std::time::{Duration, SystemTime};

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
pub const MIN_DEBOUNCE_MS_ENV: &str = "BASEMIND_DAEMON_MIN_DEBOUNCE_MS";

pub const HEAVY_CONCURRENCY_ENV: &str = "BASEMIND_DAEMON_HEAVY_CONCURRENCY";
pub const MEDIUM_CONCURRENCY_ENV: &str = "BASEMIND_DAEMON_MEDIUM_CONCURRENCY";
pub const QUEUE_WAIT_MS_ENV: &str = "BASEMIND_DAEMON_QUEUE_WAIT_MS";

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
/// Daemon ceiling for `[documents] phase_budget_secs` and `max_abandoned_extractions`: a repo's
/// `basemind.toml` can tighten them but not stretch the document phase or the stuck-thread pile.
const DAEMON_MAX_PHASE_SECS: u64 = 7_200;
const DAEMON_MAX_ABANDONED_EXTRACTIONS: usize = 16;
const DEFAULT_MAX_CHUNKS_PER_DOC: usize = 20_000;
/// A `0` debounce makes every filesystem event trigger its own rescan.
const DEFAULT_MIN_DEBOUNCE_MS: u64 = 50;

/// Fewest heavy or medium calls the core-count default allows at once.
const DEFAULT_CONCURRENCY_MIN: usize = 2;
/// Most heavy calls the core-count default allows at once: one heavy call (a rescan, an embedding
/// search) can occupy the daemon's whole rayon pool, so more of them only queue inside it.
const DEFAULT_HEAVY_CONCURRENCY_MAX: usize = 4;
/// Most medium calls the core-count default allows at once. With the heavy maximum this stays under
/// the daemon's blocking-thread ceiling, so admitted calls never wait for a thread.
const DEFAULT_MEDIUM_CONCURRENCY_MAX: usize = 8;
/// Cores per heavy slot and per medium slot of the core-count default.
const CORES_PER_HEAVY_SLOT: usize = 4;
const CORES_PER_MEDIUM_SLOT: usize = 2;
const DEFAULT_QUEUE_WAIT_MS: u64 = 2_000;

/// How many expensive calls one MCP server runs at once, per work class, and how long a call waits
/// for a slot before it is answered `server_busy`. Heavy calls are the allocation-heavy ones
/// (`index_rescan`, `code_search`, `docs_search`); medium calls read the whole corpus or the whole
/// import graph (`code_grep`, the import-graph relations of `code_refs`, and the CLI's impact,
/// cycles, deadcode and overview). Each class has its own slots, so a rescan never holds back a grep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdmissionLimits {
    /// Heavy calls run at once (`BASEMIND_DAEMON_HEAVY_CONCURRENCY`).
    pub heavy_concurrency: usize,
    /// Medium calls run at once (`BASEMIND_DAEMON_MEDIUM_CONCURRENCY`).
    pub medium_concurrency: usize,
    /// Longest a heavy or medium call waits for a slot (`BASEMIND_DAEMON_QUEUE_WAIT_MS`).
    pub queue_wait: Duration,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self::from_vars(|_| None)
    }
}

impl AdmissionLimits {
    /// The limits `var` names, `var` returning a variable's value. Each is a positive integer; an
    /// unset, zero or unparseable one keeps its default, which for the two concurrencies is derived
    /// from this machine's core count.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Self::from_vars_on(var, cores)
    }

    fn from_vars_on(var: impl Fn(&str) -> Option<String>, cores: usize) -> Self {
        let positive = |name: &str| {
            var(name)
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .filter(|n| *n > 0)
        };
        let slots = |name: &str, per_slot: usize, max: usize| {
            positive(name).map_or_else(
                || (cores / per_slot).clamp(DEFAULT_CONCURRENCY_MIN, max),
                |n| usize::try_from(n).unwrap_or(usize::MAX),
            )
        };
        Self {
            heavy_concurrency: slots(
                HEAVY_CONCURRENCY_ENV,
                CORES_PER_HEAVY_SLOT,
                DEFAULT_HEAVY_CONCURRENCY_MAX,
            ),
            medium_concurrency: slots(
                MEDIUM_CONCURRENCY_ENV,
                CORES_PER_MEDIUM_SLOT,
                DEFAULT_MEDIUM_CONCURRENCY_MAX,
            ),
            queue_wait: Duration::from_millis(positive(QUEUE_WAIT_MS_ENV).unwrap_or(DEFAULT_QUEUE_WAIT_MS)),
        }
    }
}

/// The operator's ceilings on a daemon-loaded workspace config, each a `BASEMIND_DAEMON_MAX_*`
/// variable (or `BASEMIND_DAEMON_MIN_DEBOUNCE_MS`, a floor).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DaemonCaps {
    pub scan_threads: usize,
    pub embed_threads: usize,
    pub embed_batch: usize,
    pub concurrent_documents: usize,
    pub footprint_mb: usize,
    pub map_cache_mb: usize,
    pub candidates: usize,
    pub file_bytes: u64,
    pub document_bytes: u64,
    pub document_pages: usize,
    pub extraction_secs: u64,
    pub chunks_per_document: usize,
    pub min_debounce_ms: u64,
}

impl Default for DaemonCaps {
    fn default() -> Self {
        Self::from_vars(|_| None)
    }
}

impl DaemonCaps {
    /// The caps `var` names, `var` returning a variable's value. A cap is a positive integer; an
    /// unset, zero or unparseable one keeps its default.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let cap_u64 = |name: &str, default: u64| {
            var(name)
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(default)
        };
        let cap = |name: &str, default: usize| cap_u64(name, default as u64) as usize;
        Self {
            scan_threads: cap(MAX_SCAN_THREADS_ENV, DEFAULT_MAX_SCAN_THREADS),
            embed_threads: cap(MAX_EMBED_THREADS_ENV, DEFAULT_MAX_EMBED_THREADS),
            embed_batch: cap(MAX_EMBED_BATCH_ENV, DEFAULT_MAX_EMBED_BATCH),
            concurrent_documents: cap(MAX_CONCURRENT_DOCUMENTS_ENV, DEFAULT_MAX_CONCURRENT_DOCUMENTS),
            footprint_mb: cap(MAX_FOOTPRINT_MB_ENV, DEFAULT_MAX_FOOTPRINT_MB),
            map_cache_mb: cap(MAX_MAP_CACHE_MB_ENV, DEFAULT_MAX_MAP_CACHE_MB),
            candidates: cap(MAX_CANDIDATES_ENV, DEFAULT_MAX_CANDIDATES),
            file_bytes: cap_u64(MAX_FILE_BYTES_ENV, DEFAULT_MAX_FILE_BYTES),
            document_bytes: cap_u64(MAX_DOC_BYTES_ENV, DEFAULT_MAX_DOC_BYTES),
            document_pages: cap(MAX_DOC_PAGES_ENV, DEFAULT_MAX_DOC_PAGES),
            extraction_secs: cap_u64(MAX_EXTRACTION_SECS_ENV, DEFAULT_MAX_EXTRACTION_SECS),
            chunks_per_document: cap(MAX_CHUNKS_PER_DOC_ENV, DEFAULT_MAX_CHUNKS_PER_DOC),
            min_debounce_ms: cap_u64(MIN_DEBOUNCE_MS_ENV, DEFAULT_MIN_DEBOUNCE_MS),
        }
    }
}

/// `min(value, cap)`, with `0` (the "unbounded / auto" sentinel) resolving to `cap`.
fn bounded(value: usize, cap: usize) -> usize {
    match value {
        0 => cap,
        n => n.min(cap),
    }
}

/// Apply the installed engine configuration's daemon caps to `config` in place.
pub fn clamp_daemon_config(config: &mut Config) {
    let caps = super::engine::with(|engine| engine.daemon_caps.clone());
    clamp_with(config, &caps);
}

fn clamp_with(config: &mut Config, caps: &DaemonCaps) {
    let embed_threads = config
        .resources
        .effective_embed_threads(config.documents.embed_max_threads);
    let resources: &mut ResourcesConfig = &mut config.resources;
    resources.scan_threads = bounded(resources.scan_threads, caps.scan_threads);
    resources.embed_threads = bounded(embed_threads, caps.embed_threads);
    resources.embed_batch_size = resources.embed_batch_size.clamp(1, caps.embed_batch);
    resources.max_concurrent_documents = bounded(resources.max_concurrent_documents, caps.concurrent_documents);
    resources.max_footprint_mb = MaxFootprint::Mebibytes(match resources.max_footprint_mb {
        MaxFootprint::Mebibytes(mb) if mb > 0 => mb.min(caps.footprint_mb),
        _ => caps.footprint_mb,
    });
    resources.max_map_cache_mb = bounded(resources.max_map_cache_mb, caps.map_cache_mb);
    config.scan.max_candidates = bounded(config.scan.max_candidates, caps.candidates);
    config.scan.max_file_bytes = config.scan.max_file_bytes.min(caps.file_bytes);

    let docs = &mut config.documents;
    docs.max_file_bytes = docs.max_file_bytes.min(caps.document_bytes);
    docs.max_pages = docs.max_pages.min(caps.document_pages);
    docs.extraction_timeout_secs = docs.extraction_timeout_secs.min(caps.extraction_secs);
    docs.phase_budget_secs = docs.phase_budget_secs.min(DAEMON_MAX_PHASE_SECS);
    docs.max_abandoned_extractions = docs.max_abandoned_extractions.min(DAEMON_MAX_ABANDONED_EXTRACTIONS);
    docs.max_chunks_per_document = docs.max_chunks_per_document.min(caps.chunks_per_document);

    config.watch.debounce_ms = config.watch.debounce_ms.max(caps.min_debounce_ms);
}

/// Resolve a workspace's config for the daemon, mirroring the CLI's `load_or_default`: a missing
/// `basemind.toml` falls back to defaults; only a genuine parse / IO / validation error propagates.
/// The daemon caps are applied either way.
pub fn load_daemon(root: &Path) -> Result<Config, ConfigError> {
    let caps = super::engine::with(|engine| engine.daemon_caps.clone());
    load_daemon_with(root, &caps)
}

fn load_daemon_with(root: &Path, caps: &DaemonCaps) -> Result<Config, ConfigError> {
    let mut config = match super::load_with_layers::<super::NoOverrides>(root, None, None) {
        Ok(loaded) => loaded.config,
        Err(ConfigError::NotFound(_)) => super::default_for_root(root),
        Err(error) => return Err(error),
    };
    clamp_with(&mut config, caps);
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

    fn write_toml(dir: &Path, body: &str) {
        std::fs::write(dir.join("basemind.toml"), format!("\"$schema\" = \"v1\"\n{body}")).expect("write toml");
    }

    #[test]
    fn unbounded_repo_values_resolve_to_the_caps_through_the_real_load_path() {
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
    fn repo_document_scan_and_watch_knobs_are_clamped() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(
            dir.path(),
            "[scan]\nmax_file_bytes = 9223372036854775807\n\
             [documents]\nmax_file_bytes = 9223372036854775807\nmax_pages = 4000000000\n\
             extraction_timeout_secs = 999999\nphase_budget_secs = 999999\nmax_abandoned_extractions = 64\nmax_chunks_per_document = 4000000000\n\
             [watch]\ndebounce_ms = 0\n",
        );
        let cfg = load_daemon(dir.path()).expect("load");
        assert_eq!(cfg.scan.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
        assert_eq!(cfg.documents.max_file_bytes, DEFAULT_MAX_DOC_BYTES);
        assert_eq!(cfg.documents.max_pages, DEFAULT_MAX_DOC_PAGES);
        assert_eq!(cfg.documents.extraction_timeout_secs, DEFAULT_MAX_EXTRACTION_SECS);
        assert_eq!(cfg.documents.phase_budget_secs, DAEMON_MAX_PHASE_SECS);
        assert_eq!(
            cfg.documents.max_abandoned_extractions,
            DAEMON_MAX_ABANDONED_EXTRACTIONS
        );
        assert_eq!(cfg.documents.max_chunks_per_document, DEFAULT_MAX_CHUNKS_PER_DOC);
        assert_eq!(cfg.watch.debounce_ms, DEFAULT_MIN_DEBOUNCE_MS);
    }

    #[test]
    fn small_repo_values_are_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(
            dir.path(),
            "[scan]\nmax_file_bytes = 4096\n[documents]\nmax_pages = 7\n",
        );
        let cfg = load_daemon(dir.path()).expect("load");
        assert_eq!(cfg.scan.max_file_bytes, 4096);
        assert_eq!(cfg.documents.max_pages, 7);
    }

    #[test]
    fn auto_and_huge_footprints_are_bounded_and_small_ones_kept() {
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
        let caps = DaemonCaps::from_vars(|name| match name {
            MAX_SCAN_THREADS_ENV => Some("12".to_string()),
            MAX_FOOTPRINT_MB_ENV => Some("8192".to_string()),
            _ => None,
        });
        let dir = tempfile::tempdir().expect("tempdir");
        write_toml(
            dir.path(),
            "[resources]\nscan_threads = 10\nmax_footprint_mb = \"off\"\n",
        );
        let cfg = load_daemon_with(dir.path(), &caps).expect("load");
        assert_eq!(cfg.resources.scan_threads, 10);
        assert_eq!(cfg.resources.max_footprint_mb, MaxFootprint::Mebibytes(8192));
    }

    #[test]
    fn admission_defaults_follow_the_core_count_within_their_bounds() {
        for (cores, heavy, medium) in [(1, 2, 2), (8, 2, 4), (12, 3, 6), (16, 4, 8), (64, 4, 8)] {
            let limits = AdmissionLimits::from_vars_on(|_| None, cores);
            assert_eq!(limits.heavy_concurrency, heavy, "{cores} cores");
            assert_eq!(limits.medium_concurrency, medium, "{cores} cores");
            assert_eq!(limits.queue_wait, Duration::from_millis(DEFAULT_QUEUE_WAIT_MS));
        }
    }

    #[test]
    fn operator_env_sets_the_admission_limits() {
        let limits = AdmissionLimits::from_vars_on(
            |name| match name {
                HEAVY_CONCURRENCY_ENV => Some(" 6 ".to_string()),
                MEDIUM_CONCURRENCY_ENV => Some("1".to_string()),
                QUEUE_WAIT_MS_ENV => Some("250".to_string()),
                _ => None,
            },
            8,
        );
        assert_eq!(limits.heavy_concurrency, 6);
        assert_eq!(
            limits.medium_concurrency, 1,
            "below the default floor when the operator says so"
        );
        assert_eq!(limits.queue_wait, Duration::from_millis(250));

        let kept = AdmissionLimits::from_vars_on(
            |name| match name {
                HEAVY_CONCURRENCY_ENV => Some("0".to_string()),
                MEDIUM_CONCURRENCY_ENV => Some("many".to_string()),
                QUEUE_WAIT_MS_ENV => Some("-5".to_string()),
                _ => None,
            },
            8,
        );
        assert_eq!(
            kept,
            AdmissionLimits::from_vars_on(|_| None, 8),
            "zero or unparseable keeps the default"
        );
    }

    #[test]
    fn missing_config_still_gets_the_caps() {
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
