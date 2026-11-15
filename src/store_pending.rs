//! The files whose vector rows the file map cannot vouch for yet.
//!
//! A scan records a file's entry, flushes the file map, and only then writes the file's vector rows
//! to LanceDB. A process killed in between, or a LanceDB write that fails, would leave an entry that
//! reads as embedded over rows that do not exist, and every later scan would skip the file as
//! unchanged. So the paths whose rows are still owed are written into the file map by that same
//! flush, and leave it in a second flush once the rows are in. A scan re-queues whatever it finds
//! here from the cached blobs before it writes its own rows.

use ahash::AHashSet;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// Paths owed vector rows, per LanceDB table. Additive in `index.msgpack`: a file map written
/// before the field decodes to the empty set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingVectors {
    /// Documents owed `documents` rows.
    #[serde(default)]
    pub docs: AHashSet<RelPath>,
    /// Code files owed `code_chunks` rows.
    #[serde(default)]
    pub code: AHashSet<RelPath>,
}

impl PendingVectors {
    /// How many paths are owed rows across both tables.
    pub fn len(&self) -> usize {
        self.docs.len() + self.code.len()
    }

    /// True when no path is owed rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop `rels` from the documents set: their rows are written, or they have none to write.
    #[cfg(feature = "documents")]
    pub(crate) fn settle_docs(&mut self, rels: &[String]) {
        for rel in rels {
            self.docs.remove(bstr::BStr::new(rel.as_bytes()));
        }
    }

    /// Drop `rels` from the code set: their rows are written, or they have none to write.
    #[cfg(feature = "code-search")]
    pub(crate) fn settle_code(&mut self, rels: &[String]) {
        for rel in rels {
            self.code.remove(bstr::BStr::new(rel.as_bytes()));
        }
    }
}
