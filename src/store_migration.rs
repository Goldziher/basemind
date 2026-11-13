//! Durable retirement of a verified standalone working view.

use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(test)]
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const WORKING: &str = "working";
const BACKUP: &str = "working.migrated";
const JOURNAL: &str = "working.migration";

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
enum Phase {
    Pending,
    Committed,
}

#[derive(Deserialize, Serialize)]
struct Journal {
    format: u8,
    generation: u64,
    phase: Phase,
}

/// The caller must also hold the workspace writer lock and close standalone DB handles.
pub(crate) struct Migration<'a> {
    views: &'a Path,
    _lock: File,
}

impl<'a> Migration<'a> {
    pub(crate) fn lock(views: &'a Path) -> Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(views.join("working.migration.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        Ok(Self { views, _lock: lock })
    }

    pub(crate) fn recover_for_writer(views: &Path) -> Result<()> {
        if exists(&views.join(JOURNAL))? || exists(&views.join(BACKUP))? {
            Self::lock(views)?.recover(|_| false)?;
        }
        Ok(())
    }

    /// Verify the source and candidate before publishing an intent to retire the source.
    #[cfg(test)]
    pub(crate) fn retire(&self, generation: u64, verify: impl FnOnce() -> Result<()>) -> Result<()> {
        ensure!(self.read()?.is_none(), "migration already recorded");
        let working = self.views.join(WORKING);
        let backup = self.views.join(BACKUP);
        ensure!(
            working.is_dir() && !exists(&backup)?,
            "unexpected standalone view layout"
        );
        verify()?;
        self.write(&Journal {
            format: 1,
            generation,
            phase: Phase::Pending,
        })?;
        fs::rename(&working, &backup).context("retire standalone working view")?;
        sync_directory(self.views)
    }

    /// Confirm the routed generation opened successfully before recording completion.
    #[cfg(test)]
    pub(crate) fn commit(&self, generation: u64, verify_route: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut journal = self.read()?.context("missing migration intent")?;
        ensure!(
            journal.phase == Phase::Pending && journal.generation == generation,
            "migration identity mismatch"
        );
        ensure!(self.views.join(BACKUP).is_dir(), "standalone migration backup missing");
        verify_route()?;
        journal.phase = Phase::Committed;
        self.write(&journal)
    }

    /// Restore incomplete migrations and committed migrations that cannot be routed anymore.
    pub(crate) fn recover(&self, can_route: impl FnOnce(u64) -> bool) -> Result<()> {
        let backup = self.views.join(BACKUP);
        let working = self.views.join(WORKING);
        let Some(journal) = self.read()? else {
            ensure!(!exists(&backup)?, "migration backup exists without journal");
            return Ok(());
        };
        if journal.phase == Phase::Committed && can_route(journal.generation) {
            ensure!(backup.is_dir(), "standalone migration backup missing");
            return Ok(());
        }
        if exists(&backup)? {
            ensure!(backup.is_dir(), "standalone migration backup is not a directory");
            if exists(&working)? {
                fs::remove_dir(&working).context("working view must be empty before restoring standalone backup")?;
            }
            fs::rename(&backup, &working).context("restore standalone working view")?;
            sync_directory(self.views)?;
        }
        ensure!(working.is_dir(), "standalone view missing during migration recovery");
        fs::remove_file(self.views.join(JOURNAL))?;
        sync_directory(self.views)
    }

    fn read(&self) -> Result<Option<Journal>> {
        let bytes = match fs::read(self.views.join(JOURNAL)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let journal: Journal = serde_json::from_slice(&bytes).context("parse working migration journal")?;
        ensure!(journal.format == 1, "unsupported working migration journal");
        Ok(Some(journal))
    }

    #[cfg(test)]
    fn write(&self, journal: &Journal) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(self.views)?;
        file.write_all(&serde_json::to_vec(journal)?)?;
        file.as_file().sync_all()?;
        file.persist(self.views.join(JOURNAL))?;
        sync_directory(self.views)
    }
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(WORKING)).unwrap();
        fs::write(dir.path().join(WORKING).join("index.msgpack"), b"verified source").unwrap();
        dir
    }

    fn assert_restored(views: &Path) {
        assert_eq!(
            fs::read(views.join(WORKING).join("index.msgpack")).unwrap(),
            b"verified source"
        );
        assert!(!exists(&views.join(BACKUP)).unwrap());
        assert!(!exists(&views.join(JOURNAL)).unwrap());
    }

    #[test]
    fn failed_verification_does_not_retire_standalone() {
        let dir = fixture();
        let migration = Migration::lock(dir.path()).unwrap();
        let failure = migration.retire(1, || anyhow::bail!("rows differ")).unwrap_err();
        assert_eq!(failure.to_string(), "rows differ");
        assert_restored(dir.path());
    }

    #[test]
    fn interrupted_retirement_rolls_back_at_each_publication_boundary() {
        for renamed in [false, true] {
            let dir = fixture();
            {
                let migration = Migration::lock(dir.path()).unwrap();
                if renamed {
                    migration.retire(1, || Ok(())).unwrap();
                    fs::create_dir(dir.path().join(WORKING)).unwrap();
                } else {
                    migration
                        .write(&Journal {
                            format: 1,
                            generation: 1,
                            phase: Phase::Pending,
                        })
                        .unwrap();
                }
            }
            let migration = Migration::lock(dir.path()).unwrap();
            migration
                .recover(|_| panic!("pending migration must roll back"))
                .unwrap();
            assert_restored(dir.path());
            migration.recover(|_| unreachable!()).unwrap();
            migration.retire(2, || Ok(())).unwrap();
            migration.commit(2, || Ok(())).unwrap();
        }
    }

    #[test]
    fn committed_backup_remains_until_route_becomes_unavailable() {
        let dir = fixture();
        let migration = Migration::lock(dir.path()).unwrap();
        assert!(Migration::lock(dir.path()).is_err());
        migration.retire(7, || Ok(())).unwrap();
        assert!(migration.commit(8, || unreachable!()).is_err());
        assert!(migration.commit(7, || anyhow::bail!("route unavailable")).is_err());
        migration.commit(7, || Ok(())).unwrap();
        migration.recover(|generation| generation == 7).unwrap();
        assert!(dir.path().join(BACKUP).join("index.msgpack").is_file());
        migration.recover(|_| false).unwrap();
        assert_restored(dir.path());
    }

    #[test]
    fn writer_open_restores_standalone_before_creating_working_view() {
        use crate::store::{LockHolder, Store, VIEW_WORKING, init_isolated_cache, workspace_cache_dir};

        init_isolated_cache();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_with_holder(root.path(), VIEW_WORKING, LockHolder::Scan).unwrap();
        let views = workspace_cache_dir(root.path()).join("views");
        let sentinel = views.join(WORKING).join("recovery-sentinel");
        fs::write(&sentinel, b"standalone").unwrap();
        drop(store);
        {
            let migration = Migration::lock(&views).unwrap();
            migration.retire(7, || Ok(())).unwrap();
        }
        assert!(!sentinel.exists());
        let reopened = Store::open_with_holder(root.path(), VIEW_WORKING, LockHolder::Scan).unwrap();
        assert_eq!(fs::read(sentinel).unwrap(), b"standalone");
        assert!(reopened.index_db.is_some());
        assert!(!exists(&views.join(BACKUP)).unwrap());
        assert!(!exists(&views.join(JOURNAL)).unwrap());
    }

    #[test]
    fn recovery_preserves_a_nonempty_working_view() {
        let dir = fixture();
        let migration = Migration::lock(dir.path()).unwrap();
        migration.retire(7, || Ok(())).unwrap();
        let working = dir.path().join(WORKING);
        fs::create_dir(&working).unwrap();
        fs::write(working.join("new"), b"new data").unwrap();
        assert!(migration.recover(|_| false).is_err());
        assert_eq!(fs::read(working.join("new")).unwrap(), b"new data");
        assert!(dir.path().join(BACKUP).join("index.msgpack").is_file());
    }
}
