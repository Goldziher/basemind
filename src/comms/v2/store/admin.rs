//! What only the operator does to the store: archive a thread whoever created it.

use super::records::{Reader, backend};
use super::{FjallAgents, Txn};
use crate::comms::v2::api::{CommsError, ThreadId};

impl FjallAgents {
    /// Archive `thread` whoever created it: `basemind agent cleanup`. Returns whether it was
    /// active; archiving an archived thread changes nothing. Records no caller as seen.
    pub fn archive_any(&self, thread: &ThreadId) -> Result<bool, CommsError> {
        self.check_writable()?;
        let mut writer = self.lock();
        let reader = Reader::new(&self.ks, self.db.snapshot());
        let mut record = reader
            .thread(thread)?
            .ok_or_else(|| CommsError::ThreadNotFound { thread: thread.clone() })?;
        if record.archived {
            return Ok(false);
        }
        let mut txn = Txn {
            writer: &mut writer,
            batch: self.db.batch(),
            now: self.clock.now_micros(),
            posted: false,
        };
        txn.archive(&self.ks, thread, &mut record)?;
        let batch = txn.batch;
        drop(reader);
        batch
            .commit()
            .map_err(|error| backend("write to the comms store", error))?;
        Ok(true)
    }
}
