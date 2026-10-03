//! Embed-policy reconciliation for a full scan.
//!
//! Vector rows (`documents`, `code_chunks`) and keyword postings are derived from the config, but
//! the unchanged-file fast paths never look at LanceDB, so flipping `embed`, editing
//! `embed_include` / `embed_exclude`, or disabling a tier would leave rows behind for files that are
//! no longer eligible (and would never rebuild rows for files that became eligible). The index
//! records [`crate::config::rules::embed_policy_digest`] of the last complete scan; when it
//! differs, [`PolicyChange::reflush`] sends unchanged eligible files back through the pipeline
//! (cached blobs, no re-embedding) and [`reconcile`] deletes the rows of ineligible ones.

use crate::config::{Config, rules};
use crate::scanner_filter::Filters;
use crate::store::Store;

/// Result of comparing the recorded embed policy with the current config.
pub(crate) struct PolicyChange {
    pub digest: String,
    /// The recorded policy differs (or none was recorded): purge ineligible rows.
    pub changed: bool,
    /// A policy was recorded and it differs: rebuild eligible rows. A missing record is not a
    /// reflush, so upgrading does not reprocess every file.
    pub reflush: bool,
}

pub(crate) fn detect(store: &Store, config: &Config) -> PolicyChange {
    let digest = rules::embed_policy_digest(config);
    let stored = store.index.embed_policy.as_str();
    PolicyChange {
        changed: stored != digest,
        reflush: !stored.is_empty() && stored != digest,
        digest,
    }
}

/// Purge the rows of ineligible paths. Call once per complete full scan when `change.changed`, then
/// record the digest with [`record`] and persist the index -- but only when this returns `true`: a
/// purge that failed (store open / delete error) must stay unrecorded so the next scan retries it.
pub(crate) fn reconcile(store: &mut Store, config: &Config, filters: &Filters, scope: &str) -> bool {
    #[allow(unused_mut)]
    let mut applied = true;
    #[cfg(feature = "code-search")]
    {
        applied &= crate::scanner_code::purge_unembedded_code(store, config, filters, scope);
    }
    #[cfg(feature = "documents")]
    {
        applied &= crate::scanner_docs::purge_unembedded_documents(store, config, filters, scope);
    }
    #[cfg(not(any(feature = "code-search", feature = "documents")))]
    let _ = (store, config, filters, scope);
    applied
}

pub(crate) fn record(store: &mut Store, change: &PolicyChange) {
    store.index.embed_policy.clone_from(&change.digest);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_distinguishes_first_scan_unchanged_and_changed_policy() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
        let mut config = crate::config::default_for_root(tmp.path());

        let first = detect(&store, &config);
        assert!(first.changed, "nothing recorded yet: purge once");
        assert!(
            !first.reflush,
            "a missing record must not reprocess every file on upgrade"
        );

        record(&mut store, &first);
        let again = detect(&store, &config);
        assert!(!again.changed && !again.reflush, "recorded policy is in sync");

        config.code_search.embed = !config.code_search.embed;
        let flipped = detect(&store, &config);
        assert!(
            flipped.changed && flipped.reflush,
            "a changed policy purges and reflushes"
        );
        assert_eq!(
            store.index.embed_policy, first.digest,
            "detect alone never records; only a completed purge does"
        );
    }
}
