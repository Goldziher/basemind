//! Opening the store: creating it, checking its version, and bringing an older one forward.
//!
//! What may be created is decided before anything is. A directory holds a store when `meta`
//! carries a layout version, and then every keyspace of the layout must already exist: a missing
//! one is an error, at the current version as much as at a newer one, and is never created.
//! Keyspaces are created only where there is no store: in a new or empty directory, or in one
//! that holds nothing but empty keyspaces of this layout and no version, which is what a crash
//! in the middle of creating a store leaves behind. Rows without a version, or keyspaces this
//! layout does not have, are somebody else's data and are refused.
//!
//! "Left untouched" means that this module writes nothing to a store it refuses, or to one newer
//! than the binary. It cannot open a store without opening the Fjall database, though, and
//! Fjall then replays its journal and may flush or compact in the background, on a newer store
//! as on any other. That rewrites files without changing what any key holds.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64};

use fjall::{Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch};

use super::records::{backend, corrupt, decode_i64, decode_u64};
use super::skew::{self, SkewLog};
use super::{FjallAgents, Keyspaces, META_LAST_TS, META_POSTS, META_STORE_VER, STORE_DIR, StoreOptions, Writer};
use crate::comms::v2::api::{COMMS_STORE_VER, CommsError};

/// Block cache of the store, in bytes. Coordination traffic is small and recent.
const CACHE_BYTES: u64 = 16 * 1024 * 1024;

/// Memtable ceiling of each keyspace, in bytes: fjall's 64 MiB default times eleven keyspaces
/// would let an idle daemon hold far more than the data is worth.
const MEMTABLE_BYTES: u64 = 4 * 1024 * 1024;

/// Ceiling of the journal on disk, in bytes: fjall's minimum, against a 512 MiB default.
const JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Name of the keyspace that holds the layout version.
const META: &str = "meta";

/// Every keyspace of this layout. A store that lacks one is not opened, since asking Fjall for
/// a keyspace creates it and a store is never completed behind its writer's back.
const KEYSPACES: [&str; 11] = [
    META,
    "threads",
    "messages",
    "bodies",
    "post_order",
    "members",
    "member_threads",
    "repo_threads",
    "replays",
    "seen",
    "expiry",
];

/// Bytes of an encoded layout version.
const VERSION_LEN: usize = 4;

/// One forward-only change of the store layout, from one version to the next one that has a
/// step. It stages its rows in the batch it is given; the batch is committed together with the
/// new version, so a step is applied whole or not at all and is never run twice.
pub(super) struct Migration {
    /// The version the step starts from.
    pub from: u32,
    /// The version the store is at once the step is committed.
    pub to: u32,
    /// Stage the step's changes.
    pub run: fn(&Keyspaces, &mut OwnedWriteBatch) -> Result<(), CommsError>,
}

/// The steps that bring any older layout to [`COMMS_STORE_VER`]. None yet: version 1000 is the
/// first layout of this store.
const MIGRATIONS: &[Migration] = &[];

impl FjallAgents {
    /// Open the store in `comms_dir` with `options`, creating it at [`COMMS_STORE_VER`] when the
    /// directory holds no store.
    ///
    /// An older store is migrated first. A newer one opens and refuses writes. A store that
    /// cannot be opened, has rows but no version, lacks a keyspace of its layout, or has no
    /// migration path is a [`CommsError::Backend`] and is left as it was found: this function
    /// never deletes, renames, recreates or completes one. Opening does let Fjall recover its
    /// journal and compact, on a newer store too, which rewrites files but no key or value.
    pub fn open_with(comms_dir: &Path, options: StoreOptions) -> Result<Self, CommsError> {
        Self::open_migrating(comms_dir, options, MIGRATIONS)
    }

    pub(super) fn open_migrating(
        comms_dir: &Path,
        options: StoreOptions,
        migrations: &[Migration],
    ) -> Result<Self, CommsError> {
        let dir = comms_dir.join(STORE_DIR);
        let untouched = |what: &str| format!("{what} the comms store at {} (left untouched)", dir.display());
        std::fs::create_dir_all(&dir).map_err(|error| backend(untouched("create"), error))?;
        let db = Database::builder(&dir)
            .worker_threads(1)
            .cache_size(CACHE_BYTES)
            .max_journaling_size(JOURNAL_BYTES)
            .open()
            .map_err(|error| backend(untouched("open"), error))?;
        let keyspace = |name: &str| -> Result<Keyspace, CommsError> {
            db.keyspace(name, || {
                KeyspaceCreateOptions::default().max_memtable_size(MEMTABLE_BYTES)
            })
            .map_err(|error| backend(untouched("open"), error))
        };
        // ~keep `Database::keyspace` creates what it does not find, so whether anything may be
        // created is settled here, from what exists, before any keyspace that might not exist
        // is asked for.
        let refuse = |found: &str| CommsError::Backend {
            detail: untouched(found),
        };
        let existing: Vec<&str> = KEYSPACES.into_iter().filter(|name| db.keyspace_exists(name)).collect();
        let on_disk = if db.keyspace_exists(META) {
            stored_version(&keyspace(META)?)?
        } else {
            None
        };
        match on_disk {
            Some(version) => {
                if let Some(missing) = KEYSPACES.iter().find(|name| !existing.contains(name)) {
                    return Err(refuse(&format!(
                        "found no `{missing}` keyspace, which layout version {version} has, in"
                    )));
                }
            }
            None => {
                if db.keyspace_count() != existing.len() {
                    return Err(refuse("found keyspaces of another layout and no layout version in"));
                }
                for name in &existing {
                    let empty = keyspace(name)?
                        .is_empty()
                        .map_err(|error| backend(untouched("read"), error))?;
                    if !empty {
                        return Err(refuse(&format!("found rows in `{name}` and no layout version in")));
                    }
                }
            }
        }
        let newer = on_disk.is_some_and(|version| version > COMMS_STORE_VER);
        let ks = Keyspaces {
            meta: keyspace(META)?,
            threads: keyspace("threads")?,
            messages: keyspace("messages")?,
            bodies: keyspace("bodies")?,
            post_order: keyspace("post_order")?,
            members: keyspace("members")?,
            member_threads: keyspace("member_threads")?,
            repo_threads: keyspace("repo_threads")?,
            replays: keyspace("replays")?,
            seen: keyspace("seen")?,
            expiry: keyspace("expiry")?,
        };
        let read = |key: &[u8]| ks.meta.get(key).map_err(|error| backend(untouched("read"), error));
        let found = match on_disk {
            Some(version) => version,
            None => {
                ks.meta
                    .insert(META_STORE_VER, COMMS_STORE_VER.to_be_bytes())
                    .map_err(|error| backend(untouched("create"), error))?;
                COMMS_STORE_VER
            }
        };
        let version = migrate(&db, &ks, found, migrations)?;
        let counter = |decoded: Result<Option<i64>, CommsError>| match decoded {
            Ok(value) => Ok(value),
            // ~keep Only writes use the counters, and a newer store takes none: its encoding of
            // them may have changed without that stopping this binary from reading the rest.
            Err(_) if newer => Ok(None),
            Err(error) => Err(error),
        };
        let posts = counter(
            read(META_POSTS)?
                .map(|bytes| decode_u64("post counter", &bytes).map(|posts| posts as i64))
                .transpose(),
        )?
        .unwrap_or(0) as u64;
        let last_ts = counter(
            read(META_LAST_TS)?
                .map(|bytes| decode_i64("timestamp", &bytes))
                .transpose(),
        )?
        .unwrap_or(i64::MIN);
        if let Some(behind_micros) = skew::clock_behind(options.clock.now_micros(), last_ts) {
            skew::warn(behind_micros, "opening the store");
        }
        Ok(Self {
            db,
            ks,
            clock: options.clock,
            writer: Mutex::new(Writer {
                posts,
                last_ts,
                thread_ids: options.thread_ids,
                swept_at: i64::MIN,
                skew: SkewLog::default(),
            }),
            arrivals: AtomicU64::new(posts),
            store_ver: AtomicU32::new(version),
        })
    }
}

/// The layout version in `meta`, when it holds one.
fn stored_version(meta: &Keyspace) -> Result<Option<u32>, CommsError> {
    let Some(bytes) = meta
        .get(META_STORE_VER)
        .map_err(|error| backend("read the layout version", error))?
    else {
        return Ok(None);
    };
    let bytes: [u8; VERSION_LEN] = bytes.as_ref().try_into().map_err(|_| corrupt("layout version"))?;
    Ok(Some(u32::from_be_bytes(bytes)))
}

/// Run every step from `version` up to [`COMMS_STORE_VER`], committing each with the version it
/// reaches. Returns the version the store is at, which for a newer store is the one found.
fn migrate(db: &Database, ks: &Keyspaces, mut version: u32, migrations: &[Migration]) -> Result<u32, CommsError> {
    while version < COMMS_STORE_VER {
        let step = migrations
            .iter()
            .find(|step| step.from == version && step.to > version)
            .ok_or_else(|| CommsError::Backend {
                detail: format!(
                    "the comms store is at layout version {version} and this basemind has no migration from it to \
                     {COMMS_STORE_VER} (left untouched)"
                ),
            })?;
        let mut batch = db.batch();
        (step.run)(ks, &mut batch)?;
        batch.insert(&ks.meta, META_STORE_VER, step.to.to_be_bytes());
        batch
            .commit()
            .map_err(|error| backend(format_args!("migrate the comms store to version {}", step.to), error))?;
        version = step.to;
    }
    Ok(version)
}
