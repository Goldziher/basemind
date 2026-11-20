//! Reaping blob namespaces this binary does not write.
//!
//! Blobs live under `cache/blobs/v<SCHEMA_VER>/` (see [`crate::store_layout::global_blobs_dir`]), so
//! two installed basemind minors never read or overwrite each other's blobs. What is left behind is
//! garbage only this sweep collects: the un-namespaced blobs of builds from before the namespaces,
//! and the namespaces of older blob schemas. An older basemind may still be in use, though: the
//! plugin launcher keeps a version for [`PRUNE_AFTER_DAYS_ENV`] days after its last launch, and its
//! daemon keeps running through an upgrade. So a superseded namespace is reaped only when
//! [`may_reap`] says so: no live basemind process reads it ([`Holds::live`]), and nothing was
//! written into it for the hold ([`Holds::hold`]). Even then a file goes only once it is older than
//! the blob grace period, the same age a scan's unreferenced blobs must reach. A namespace of a
//! NEWER schema belongs to a newer basemind sharing the cache and is never touched.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::data_root::DataRoot;
use crate::store_gc::{GcError, GcReport, read_dir};

/// How long, in whole days, an older basemind's blob namespace is kept after its last write (default
/// 14, `0` drops the hold). The plugin launcher reads the same variable for how long it keeps an
/// unused version installed.
pub const PRUNE_AFTER_DAYS_ENV: &str = "BASEMIND_PRUNE_AFTER_DAYS";

/// The launcher's default retention of an unused version (`plugin/scripts/mcp-launch.sh`).
const DEFAULT_PRUNE_AFTER_DAYS: u64 = 14;

const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// A [`PRUNE_AFTER_DAYS_ENV`] value in days, defaulting to 14. An unparseable value is logged and
/// falls back to the default, as the launcher does.
pub fn prune_hold_from(raw: Option<&str>) -> Duration {
    let days = match raw {
        Some(raw) => raw.trim().parse::<u64>().unwrap_or_else(|_| {
            crate::config::trust::warn_once(format!(
                "{PRUNE_AFTER_DAYS_ENV}={raw:?} is not a number; holding older blob namespaces for the default"
            ));
            DEFAULT_PRUNE_AFTER_DAYS
        }),
        None => DEFAULT_PRUNE_AFTER_DAYS,
    };
    Duration::from_secs(days.saturating_mul(SECS_PER_DAY))
}

/// Where a basemind reads its blobs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobNamespace {
    /// Directly in the blobs root: every release up to 0.33.
    Unnamespaced,
    /// `v<N>/`, `N` being the blob schema.
    Schema(u16),
}

/// The namespace a basemind of `version` reads. Releases up to 0.33, the only ones that record their
/// daemon directly in the registry root (`pre_generation`), read the un-namespaced blobs; a later
/// one reads the namespace of its release minor, `M.X.y` counted as `M * 100 + X` like
/// [`crate::version::RELEASE_MINOR`]. `None` for a version that does not parse.
pub(crate) fn namespace_read_by(version: &str, pre_generation: bool) -> Option<BlobNamespace> {
    if pre_generation {
        return Some(BlobNamespace::Unnamespaced);
    }
    let mut parts = version.split('.');
    let major: u16 = parts.next()?.parse().ok()?;
    let minor: u16 = parts.next()?.parse().ok()?;
    major.checked_mul(100)?.checked_add(minor).map(BlobNamespace::Schema)
}

/// The namespace a basemind reads, from what its registry record or hosting marker says: the blob
/// schema a build after 0.33 records, else [`namespace_read_by`] its version. The recorded schema
/// wins: a build's package version is bumped only at release, so between releases it can name a
/// minor whose namespace the build does not read.
pub(crate) fn namespace_recorded(
    blob_schema: Option<u16>,
    version: Option<&str>,
    pre_generation: bool,
) -> Option<BlobNamespace> {
    match blob_schema {
        Some(schema) => Some(BlobNamespace::Schema(schema)),
        None => namespace_read_by(version?, pre_generation),
    }
}

/// What keeps a superseded namespace on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Holds {
    /// The namespaces a live basemind process reads.
    pub(crate) live: Vec<BlobNamespace>,
    /// Whether a live basemind process reads a namespace its record does not tell: a hosting
    /// marker that names no build (one written before markers recorded it, or one that does not
    /// parse), or a version that does not parse. Such a process may read any superseded namespace,
    /// so it holds them all: the superseded sweep reaps nothing while it runs. The cost is disk
    /// only, and lasts no longer than the process at that pid. A daemon registry that could not be
    /// read whole ([`crate::daemon_lock::DaemonCensus::complete`]) may hide such a process, and
    /// holds them all the same until it reads whole again.
    pub(crate) unidentified: bool,
    /// How long after the last write into it a namespace is kept.
    pub(crate) hold: Duration,
}

impl Holds {
    /// The holds under the data home `data`: the namespaces of the live daemons and hosts
    /// registered there, and the engine configuration's hold.
    pub(crate) fn under(data: &DataRoot) -> Self {
        let read = live_namespaces(data);
        Self {
            unidentified: read.contains(&None),
            live: read.into_iter().flatten().collect(),
            hold: crate::config::engine::with(|config| config.gc.superseded_hold),
        }
    }
}

/// The namespaces the live daemons (`daemons/`, every generation) and hosts (`hosting/<pid>`)
/// registered under the data home `data` read, `None` for one whose namespace cannot be told, and a
/// `None` for a daemon registry that could not be read whole. A process that refuses to be opened
/// counts as live ([`crate::boot_identity::PidState::may_be_live`]): the hold gates a deletion.
#[cfg(any(unix, windows))]
fn live_namespaces(data: &DataRoot) -> Vec<Option<BlobNamespace>> {
    let census = crate::daemon_lock::registered_daemons(data);
    let unread = (!census.complete).then_some(None);
    let daemons = census.daemons.into_iter().map(|daemon| {
        let record = daemon.record;
        namespace_recorded(record.blob_schema, Some(&record.version), daemon.pre_generation)
    });
    let hosts = crate::scan_evidence_hosts::live_host_builds(data)
        .into_iter()
        .map(|host| namespace_recorded(host.blob_schema, host.version.as_deref(), false));
    daemons.chain(hosts).chain(unread).collect()
}

#[cfg(not(any(unix, windows)))]
fn live_namespaces(_data: &DataRoot) -> Vec<Option<BlobNamespace>> {
    Vec::new()
}

/// Whether a superseded `namespace` may be reaped: no live basemind process reads it, and its newest
/// file was written at least `hold` ago.
pub(crate) fn may_reap(
    namespace: BlobNamespace,
    newest_write_age: Duration,
    live: &[BlobNamespace],
    hold: Duration,
) -> bool {
    !live.contains(&namespace) && newest_write_age >= hold
}

/// Whether a file name in the blobs root is a blob an older build wrote there: a current blob
/// suffix, or one of the dead pre-0.9 ones. Anything else in the root is not this sweep's to take.
fn is_blob_file(name: &str) -> bool {
    crate::store_gc::blob_stem(name).is_some() || name.ends_with(".l1.msgpack") || name.ends_with(".l2.msgpack")
}

/// The blob schema a namespace directory name (`v<N>`) stands for.
pub(crate) fn namespace_schema(name: &str) -> Option<u16> {
    name.strip_prefix('v')?.parse().ok()
}

/// How long ago `modified` was, zero when it is unknown or in the future.
fn age_of(modified: std::io::Result<SystemTime>, now: SystemTime) -> Duration {
    modified
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or(Duration::ZERO)
}

/// How long ago the newest of `files` was written; [`Duration::MAX`] when there is none. A file whose
/// age cannot be read counts as just written.
fn newest_write_age(files: &[PathBuf], now: SystemTime) -> Duration {
    files
        .iter()
        .map(|file| age_of(std::fs::metadata(file).and_then(|meta| meta.modified()), now))
        .min()
        .unwrap_or(Duration::MAX)
}

/// The files directly in `dir`.
fn files_in(dir: &Path) -> Result<Vec<PathBuf>, GcError> {
    let mut files = Vec::new();
    for entry in read_dir(dir)? {
        let path = entry
            .map_err(|source| GcError::Io {
                path: dir.to_path_buf(),
                source,
            })?
            .path();
        if !path.is_dir() {
            files.push(path);
        }
    }
    Ok(files)
}

/// Whether the entry `name` of the blobs root is what [`sweep_superseded_namespaces`] reaps once it
/// is aged, given the `current` blob schema: the namespace directory of an older schema, or an
/// un-namespaced blob file. Shared with `basemind upgrade --plan` ([`crate::upgrade_plan`]), so the
/// plan names exactly what the sweep takes.
pub(crate) fn superseded(name: &str, is_dir: bool, current: u16) -> bool {
    if is_dir {
        namespace_schema(name).is_some_and(|schema| schema < current)
    } else {
        is_blob_file(name)
    }
}

/// Remove `path` when it is at least `grace` old. Returns whether the file is gone.
fn reap_if_aged(path: &Path, now: SystemTime, grace: Duration, report: &mut GcReport) -> Result<bool, GcError> {
    let io = |source| GcError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => return Err(io(source)),
    };
    report.scanned += 1;
    if age_of(meta.modified(), now) < grace {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(io)?;
    report.removed += 1;
    report.bytes_freed += meta.len();
    Ok(true)
}

/// Reap the aged files of one superseded namespace, and the directory once it is empty.
fn reap_namespace(dir: &Path, now: SystemTime, grace: Duration, report: &mut GcReport) -> Result<(), GcError> {
    let mut emptied = true;
    for entry in read_dir(dir)? {
        let path = entry
            .map_err(|source| GcError::Io {
                path: dir.to_path_buf(),
                source,
            })?
            .path();
        if path.is_dir() || !reap_if_aged(&path, now, grace, report)? {
            emptied = false;
        }
    }
    if emptied {
        // Best effort: a writer of that version may have dropped a blob in since the listing.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// Reap every aged blob under `blobs_root` that the `current` schema's namespace does not own and
/// `holds` lets go ([`may_reap`]): un-namespaced blob files and the namespaces of older schemas. Newer
/// namespaces, the current one and anything unrecognised, a file in the root that is not a blob
/// included, are left alone, and so is everything while a live process's namespace cannot be told
/// ([`Holds::unidentified`]).
pub(crate) fn sweep_superseded_namespaces(
    blobs_root: &Path,
    current: u16,
    grace: Duration,
    holds: &Holds,
) -> Result<GcReport, GcError> {
    let now = SystemTime::now();
    let mut report = GcReport::default();
    if holds.unidentified || !blobs_root.exists() {
        return Ok(report);
    }
    let releasable = |namespace: BlobNamespace, files: &[PathBuf]| {
        may_reap(namespace, newest_write_age(files, now), &holds.live, holds.hold)
    };
    let mut unnamespaced = Vec::new();
    for entry in read_dir(blobs_root)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: blobs_root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let is_dir = path.is_dir();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !superseded(&name, is_dir, current) {
            continue;
        }
        if is_dir {
            if let Some(schema) = namespace_schema(&name)
                && releasable(BlobNamespace::Schema(schema), &files_in(&path)?)
            {
                reap_namespace(&path, now, grace, &mut report)?;
            }
        } else {
            unnamespaced.push(path);
        }
    }
    if releasable(BlobNamespace::Unnamespaced, &unnamespaced) {
        for path in &unnamespaced {
            reap_if_aged(path, now, grace, &mut report)?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: u16 = 7;

    const DAY: Duration = Duration::from_secs(SECS_PER_DAY);

    fn plant(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"blob").unwrap();
        path
    }

    /// Plant a blob last written `age` ago.
    fn plant_aged(root: &Path, rel: &str, age: Duration) -> PathBuf {
        let path = plant(root, rel);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(SystemTime::now() - age))
            .unwrap();
        path
    }

    /// A row of `a_superseded_namespace_is_reaped_only_when_unused_and_past_the_hold`: the case,
    /// the namespace, the days since its newest write, the live namespaces, the hold in days, and
    /// whether it is reaped.
    type ReapCase = (&'static str, BlobNamespace, u32, &'static [BlobNamespace], u32, bool);

    #[test]
    fn a_superseded_namespace_is_reaped_only_when_unused_and_past_the_hold() {
        use BlobNamespace::{Schema, Unnamespaced};
        let cases: &[ReapCase] = &[
            ("unused, past the hold", Schema(6), 15, &[], 14, true),
            ("written the hold ago exactly", Schema(6), 14, &[], 14, true),
            ("written within the hold", Schema(6), 13, &[], 14, false),
            ("read by a live process", Schema(6), 15, &[Schema(6)], 14, false),
            ("only another namespace is live", Schema(6), 15, &[Schema(5)], 14, true),
            (
                "a live 0.33 holds the un-namespaced",
                Unnamespaced,
                15,
                &[Unnamespaced],
                14,
                false,
            ),
            (
                "a live namespaced one does not",
                Unnamespaced,
                15,
                &[Schema(6)],
                14,
                true,
            ),
            ("no hold still spares a live one", Schema(6), 0, &[Schema(6)], 0, false),
            ("no hold reaps a fresh unused one", Schema(6), 0, &[], 0, true),
        ];
        for (case, namespace, age_days, live, hold_days, expected) in cases {
            assert_eq!(
                may_reap(*namespace, *age_days * DAY, live, *hold_days * DAY),
                *expected,
                "{case}"
            );
        }
    }

    #[test]
    fn a_version_names_the_namespace_it_reads() {
        let cases: &[(&str, bool, Option<BlobNamespace>)] = &[
            ("0.33.0", true, Some(BlobNamespace::Unnamespaced)),
            ("0.27.4", true, Some(BlobNamespace::Unnamespaced)),
            ("garbage", true, Some(BlobNamespace::Unnamespaced)),
            ("0.34.0", false, Some(BlobNamespace::Schema(34))),
            ("0.35.0-rc.1", false, Some(BlobNamespace::Schema(35))),
            ("1.2.3", false, Some(BlobNamespace::Schema(102))),
            ("0", false, None),
            ("garbage", false, None),
            ("700.0.0", false, None),
        ];
        for (version, pre_generation, expected) in cases {
            assert_eq!(
                namespace_read_by(version, *pre_generation),
                *expected,
                "{version} (pre-generation: {pre_generation})"
            );
        }
    }

    #[test]
    fn a_recorded_blob_schema_names_the_namespace_before_the_version_does() {
        type Case<'a> = (Option<u16>, Option<&'a str>, bool, Option<BlobNamespace>);
        let cases: &[Case] = &[
            (Some(34), Some("0.33.0"), false, Some(BlobNamespace::Schema(34))),
            (Some(34), None, false, Some(BlobNamespace::Schema(34))),
            (None, Some("0.33.0"), true, Some(BlobNamespace::Unnamespaced)),
            (None, Some("0.34.0"), false, Some(BlobNamespace::Schema(34))),
            (None, None, false, None),
        ];
        for (blob_schema, version, pre_generation, expected) in cases {
            assert_eq!(
                namespace_recorded(*blob_schema, *version, *pre_generation),
                *expected,
                "{blob_schema:?} {version:?} (pre-generation: {pre_generation})"
            );
        }
    }

    /// The live set is read from the registry of the data home the sweep is given, not the
    /// process's: an empty home holds nothing. A daemon this build registers holds the namespace it
    /// reads whatever its version string says, and a 0.33 record in the registry root holds the
    /// un-namespaced blobs.
    #[cfg(any(unix, windows))]
    #[test]
    fn the_live_set_is_the_swept_home_s_and_this_build_holds_the_namespace_it_reads() {
        use crate::daemon_lock::{DaemonKind, DaemonLock, DaemonLockOutcome};
        let home = tempfile::tempdir().unwrap();
        let data = DataRoot::new(home.path());
        assert_eq!(Holds::under(&data).live, Vec::<BlobNamespace>::new());

        let comms = tempfile::tempdir().unwrap();
        let registry = home.path().join("daemons");
        let outcome = DaemonLock::acquire_at(DaemonKind::Comms, comms.path(), "0.20.0", &registry.join("g1")).unwrap();
        assert!(matches!(outcome, DaemonLockOutcome::Acquired(_)));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let legacy = serde_json::json!({
            "pid": std::process::id(), "kind": "comms", "dir": "/tmp/comms", "version": "0.33.0", "started_unix": now,
        });
        std::fs::write(registry.join("comms-legacy.pid"), legacy.to_string()).unwrap();

        assert_eq!(
            Holds::under(&data).live,
            [
                BlobNamespace::Unnamespaced,
                BlobNamespace::Schema(crate::extract::SCHEMA_VER)
            ]
        );
    }

    /// A daemon record that cannot be read may be a live daemon's: the census is incomplete and
    /// the superseded sweep reaps nothing. A registry that does not exist is complete and empty.
    #[cfg(any(unix, windows))]
    #[test]
    fn an_unreadable_daemon_record_holds_every_superseded_namespace() {
        let home = tempfile::tempdir().unwrap();
        let data = DataRoot::new(home.path());
        assert!(!Holds::under(&data).unidentified, "no registry, nothing unread");
        std::fs::create_dir_all(home.path().join("daemons/g1/comms-unreadable.pid")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let older = plant_aged(root.path(), "v6/bb.fm.msgpack", 30 * DAY);

        let holds = Holds::under(&data);
        assert_eq!(holds.live, Vec::<BlobNamespace>::new());
        assert!(holds.unidentified, "the unread record may be a live daemon's");
        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::ZERO, &holds).unwrap();
        assert_eq!((report.scanned, report.removed), (0, 0));
        assert!(
            older.exists(),
            "nothing is reaped while the registry cannot be read whole"
        );
    }

    /// A live host whose marker names no build, as one written before markers recorded it, may
    /// read any superseded namespace: while it runs the superseded sweep reaps nothing.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_live_host_of_an_unknown_build_holds_every_superseded_namespace() {
        let home = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let marker = serde_json::json!({
            "boot": crate::boot_identity::current(), "started": crate::boot_identity::process_start(me),
        });
        let hosting = home.path().join("hosting");
        std::fs::create_dir_all(&hosting).unwrap();
        std::fs::write(hosting.join(me.to_string()), marker.to_string()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let legacy = plant_aged(root.path(), "aa.fm.msgpack", 30 * DAY);
        let older = plant_aged(root.path(), "v6/bb.fm.msgpack", 30 * DAY);

        let holds = Holds::under(&DataRoot::new(home.path()));
        assert_eq!(holds.live, Vec::<BlobNamespace>::new());
        assert!(holds.unidentified, "the host's namespace cannot be told");
        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::ZERO, &holds).unwrap();
        assert_eq!((report.scanned, report.removed), (0, 0));
        assert!(legacy.exists() && older.exists(), "nothing is reaped under the host");

        let identified = Holds {
            unidentified: false,
            ..holds
        };
        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::ZERO, &identified).unwrap();
        assert_eq!(report.removed, 2, "both are reaped once no such host runs");
    }

    /// The holds are read once the current namespace is swept: a basemind that registered while it
    /// was swept keeps the namespace it reads.
    #[test]
    fn the_holds_are_read_after_the_current_namespace_is_swept() {
        let root = tempfile::tempdir().unwrap();
        let workspaces = tempfile::tempdir().unwrap();
        let orphan = plant(root.path(), "v7/aa.fm.msgpack");
        let held = plant_aged(root.path(), "v6/bb.fm.msgpack", 30 * DAY);

        let report = crate::store_gc::gc_blob_store_in(workspaces.path(), root.path(), CURRENT, Duration::ZERO, || {
            assert!(!orphan.exists(), "the current namespace is swept first");
            Holds {
                live: vec![BlobNamespace::Schema(6)],
                unidentified: false,
                hold: Duration::ZERO,
            }
        })
        .unwrap();

        assert_eq!(report.removed, 1);
        assert!(held.exists(), "the basemind registered during the first sweep reads v6");
    }

    #[test]
    fn the_hold_is_read_in_days_and_defaults_to_the_launcher_s_fourteen() {
        assert_eq!(prune_hold_from(None), 14 * DAY);
        assert_eq!(prune_hold_from(Some("3")), 3 * DAY);
        assert_eq!(prune_hold_from(Some(" 0 ")), Duration::ZERO);
        assert_eq!(prune_hold_from(Some("two weeks")), 14 * DAY);
    }

    /// A 0.33 daemon still running in another repository reads the un-namespaced blobs, and one of
    /// an older namespace reads that namespace: neither is reaped under them, whatever their age.
    #[test]
    fn a_namespace_a_live_process_reads_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let legacy = plant_aged(root.path(), "aa.fm.msgpack", 30 * DAY);
        let held = plant_aged(root.path(), "v5/bb.fm.msgpack", 30 * DAY);
        let free = plant_aged(root.path(), "v6/cc.fm.msgpack", 30 * DAY);
        let holds = Holds {
            live: vec![BlobNamespace::Unnamespaced, BlobNamespace::Schema(5)],
            unidentified: false,
            hold: Duration::ZERO,
        };

        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::ZERO, &holds).unwrap();

        assert_eq!(report.removed, 1);
        assert!(legacy.exists(), "a live 0.33 reads the un-namespaced blobs");
        assert!(held.exists(), "a live process reads v5");
        assert!(!free.exists(), "nothing live reads v6");
    }

    /// The hold is judged by a namespace's newest file: one blob written recently keeps the whole
    /// namespace, so a version still in use is not left with half its blobs.
    #[test]
    fn a_namespace_written_within_the_hold_is_kept_whole() {
        let root = tempfile::tempdir().unwrap();
        let stale = plant_aged(root.path(), "v6/aa.fm.msgpack", 30 * DAY);
        let recent = plant_aged(root.path(), "v6/bb.fm.msgpack", 2 * DAY);
        let legacy_stale = plant_aged(root.path(), "cc.fm.msgpack", 30 * DAY);
        let legacy_recent = plant_aged(root.path(), "dd.fm.msgpack", 2 * DAY);
        let expired = plant_aged(root.path(), "v5/ee.fm.msgpack", 20 * DAY);
        let holds = Holds {
            live: Vec::new(),
            unidentified: false,
            hold: 14 * DAY,
        };

        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::ZERO, &holds).unwrap();

        assert_eq!(report.removed, 1);
        assert!(stale.exists() && recent.exists(), "v6 was written two days ago");
        assert!(
            legacy_stale.exists() && legacy_recent.exists(),
            "so were the un-namespaced blobs"
        );
        assert!(
            !expired.exists() && !root.path().join("v5").exists(),
            "v5 is past the hold"
        );
    }

    /// Only what an older build left behind is garbage: the current namespace is the live store and
    /// a newer one belongs to a newer basemind sharing the cache.
    #[test]
    fn only_unnamespaced_blobs_and_older_namespaces_are_reaped() {
        let root = tempfile::tempdir().unwrap();
        let legacy = plant(root.path(), "aa.fm.msgpack");
        let older = plant(root.path(), "v6/bb.fm.msgpack");
        let current = plant(root.path(), "v7/cc.fm.msgpack");
        let newer = plant(root.path(), "v8/dd.fm.msgpack");
        let unknown = plant(root.path(), "scratch/ee.fm.msgpack");
        let not_a_blob = plant(root.path(), "gc-notes.json");
        let dead_format = plant(root.path(), "ff.l1.msgpack");

        let report = sweep_superseded_namespaces(root.path(), CURRENT, Duration::ZERO, &Holds::default()).unwrap();

        assert_eq!(report.removed, 3);
        assert!(!legacy.exists(), "an un-namespaced blob is garbage");
        assert!(!dead_format.exists(), "so is one of a dead blob format");
        assert!(
            not_a_blob.exists(),
            "a file in the root that is not a blob is not ours to reap"
        );
        assert!(
            !older.exists() && !root.path().join("v6").exists(),
            "an older namespace goes"
        );
        assert!(current.exists(), "the current namespace is never swept here");
        assert!(newer.exists(), "a newer basemind's namespace is never touched");
        assert!(unknown.exists(), "an unrecognised directory is left alone");
    }

    #[test]
    fn nothing_younger_than_the_grace_period_is_reaped() {
        let root = tempfile::tempdir().unwrap();
        let legacy = plant(root.path(), "aa.fm.msgpack");
        let older = plant(root.path(), "v6/bb.fm.msgpack");

        let report =
            sweep_superseded_namespaces(root.path(), CURRENT, Duration::from_secs(3600), &Holds::default()).unwrap();

        assert_eq!((report.scanned, report.removed), (2, 0));
        assert!(legacy.exists() && older.exists());
    }
}
