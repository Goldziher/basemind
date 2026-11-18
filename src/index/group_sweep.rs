//! The group sweep (ADR-0028 rule 10, ADR-0036 rules 8 and 9): deletes the content-keyed rows no
//! view of a repository group holds any more, and the rows of views that are gone.
//!
//! Symbol and call rows are keyed by content and shared by every view; no scan deletes the rows of a
//! content it stops holding. The sweep does, at the end of a full pass that finds the group owing
//! one ([`META_CONTENT_SWEEP_OWED`], set in the batch of every path write) or a view retired. A
//! content is live while a file map of the group names it (the caller reads them), or while a
//! view's writer has staged it and no file map that view wrote names it yet: the pending set.
//!
//! Concurrency, all within the process holding the group lock (no other process writes the group):
//!
//! - A writer registers a content as pending, and reads the rows stored under it, while it holds the
//!   group gate shared ([`super::IndexDb::bm25_gate`]); the sweep re-checks the pending set and
//!   commits each delete batch holding it exclusively. So a write either registered before the
//!   check, and its content is kept, or reads the rows after the delete and writes them again.
//! - A file map written while a sweep runs may name a content the sweep read no map naming: its
//!   pending entry outlives that write until the sweep ends ([`PendingContents::settle`]).
//! - The debt is cleared only when no view has a pending content: a pass still running may write a
//!   file map that drops one.
//!
//! A view is retired when its view directory is gone (the workspace reaper or the cache budget
//! removed it) and no handle of this process has it open. Its record is replaced by a tombstone in
//! one synced commit, so no open allocates its id again; its rows are deleted; then the tombstone
//! goes. A sweep finishes the tombstones a crash left first.

use std::sync::Arc;

use ahash::AHashMap;
use fjall::PersistMode;

use super::group::{GroupDb, SHARED_PARTS, view_record};
use super::keys::{CONTENT_KEY_LEN, ContentKey};
use super::scoped::Scoped;
use super::view::Part;
use super::{IndexDb, IndexError};

/// Group `meta`: a token present while the shared symbol and call rows may hold a content no view
/// names.
pub(crate) const META_CONTENT_SWEEP_OWED: &[u8] = b"\xffgroup/content_sweep_owed";
/// Group `meta`: `<prefix><view id>`, a retired view whose rows may not all be deleted yet.
const META_RETIRED_VIEW: &[u8] = b"\xffgroup/retired_view/";
/// Contents deleted per commit, each batch under the gate held exclusively.
const SWEEP_BATCH_CONTENTS: usize = 256;

/// Content keys staged by each view's writer that no file map the view wrote names yet. See the
/// module docs.
#[derive(Default)]
pub(crate) struct PendingContents {
    seq: u64,
    /// Per view: each pending key with the sequence number of its latest registration.
    by_view: AHashMap<u32, AHashMap<ContentKey, u64>>,
    sweeping: bool,
    /// Views that wrote a file map while the sweep ran, with the sequence number then: their keys
    /// registered up to it are settled when the sweep ends.
    settled_while_sweeping: AHashMap<u32, u64>,
}

impl PendingContents {
    fn register(&mut self, view_id: u32, key: ContentKey) {
        self.seq += 1;
        self.by_view.entry(view_id).or_default().insert(key, self.seq);
    }

    /// The view wrote its file map: every key it staged so far is named by it, or was dropped.
    fn settle(&mut self, view_id: u32) {
        if self.sweeping {
            self.settled_while_sweeping.insert(view_id, self.seq);
        } else {
            self.by_view.remove(&view_id);
        }
    }

    fn holds(&self, key: &ContentKey) -> bool {
        self.by_view.values().any(|keys| keys.contains_key(key))
    }

    fn is_empty(&self) -> bool {
        self.by_view.values().all(|keys| keys.is_empty())
    }

    fn end_sweep(&mut self) {
        self.sweeping = false;
        for (view_id, upto) in std::mem::take(&mut self.settled_while_sweeping) {
            if let Some(keys) = self.by_view.get_mut(&view_id) {
                keys.retain(|_, seq| *seq > upto);
            }
        }
        self.by_view.retain(|_, keys| !keys.is_empty());
    }
}

impl GroupDb {
    fn pending(&self) -> std::sync::MutexGuard<'_, PendingContents> {
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `key` was staged by view `view_id`'s writer.
    pub(crate) fn register_pending(&self, view_id: u32, key: ContentKey) {
        self.pending().register(view_id, key);
    }

    /// View `view_id` wrote its file map.
    pub(crate) fn settle_pending(&self, view_id: u32) {
        self.pending().settle(view_id);
    }

    /// Open the sweep window, or `None` while another sweep holds it. Pending keys of views no
    /// handle has open are dropped first: their writers are gone, and so is any file map they did
    /// not write.
    fn begin_sweep(self: &Arc<Self>) -> Option<SweepWindow> {
        let open = self.live_key_views();
        let mut pending = self.pending();
        if pending.sweeping {
            return None;
        }
        pending.by_view.retain(|view_id, _| open.contains(view_id));
        pending.sweeping = true;
        Some(SweepWindow {
            group: Arc::clone(self),
        })
    }
}

/// Ends the sweep window on drop.
struct SweepWindow {
    group: Arc<GroupDb>,
}

impl Drop for SweepWindow {
    fn drop(&mut self) {
        self.group.pending().end_sweep();
    }
}

/// What one group sweep removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupSweepReport {
    /// Views whose record and rows were deleted.
    pub views_retired: usize,
    /// Contents whose symbol and call rows were deleted.
    pub contents_removed: usize,
    /// The shared BM25 posting list was swept for the retired views' postings.
    pub postings_swept: bool,
}

/// An open group sweep; see the module docs. The caller reads the file maps after opening it.
pub(crate) struct GroupSweep<'a> {
    db: &'a IndexDb,
    group: Arc<GroupDb>,
    _window: SweepWindow,
    /// The debt as the sweep found it.
    owed: Option<u64>,
    report: GroupSweepReport,
    /// A retired view held BM25 forward rows, so the posting list owes a sweep.
    postings_owed: bool,
}

/// A token no earlier one in this process equals and no other process plausibly picks, so a row
/// holding it was set by this call.
pub(crate) fn fresh_token() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let mixed = nanos ^ u64::from(std::process::id()).rotate_left(40);
    mixed
        .wrapping_add(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        .max(1)
}

fn read_u64(bytes: &[u8]) -> Option<u64> {
    <[u8; 8]>::try_from(bytes).ok().map(u64::from_be_bytes)
}

fn retired_view_key(view_id: u32) -> Vec<u8> {
    let mut key = META_RETIRED_VIEW.to_vec();
    key.extend_from_slice(&view_id.to_be_bytes());
    key
}

impl IndexDb {
    fn content_sweep_owed(&self) -> Result<Option<u64>, IndexError> {
        Ok(self
            .meta
            .raw()
            .get(META_CONTENT_SWEEP_OWED)?
            .as_deref()
            .and_then(read_u64))
    }

    /// Open a sweep of this view's group, or `None` for a database of its own or while another sweep
    /// of the group runs. Finishes the retirements a crash left half done.
    pub(crate) fn begin_group_sweep(&self) -> Result<Option<GroupSweep<'_>>, IndexError> {
        let Some(own) = &self.group else {
            return Ok(None);
        };
        let Some(window) = own.group.begin_sweep() else {
            return Ok(None);
        };
        let mut sweep = GroupSweep {
            db: self,
            group: Arc::clone(&own.group),
            _window: window,
            owed: self.content_sweep_owed()?,
            report: GroupSweepReport::default(),
            postings_owed: false,
        };
        let mut leftovers = Vec::new();
        for guard in sweep.group.meta().prefix(META_RETIRED_VIEW) {
            let key = guard.key()?;
            if let Some(id) = key
                .get(META_RETIRED_VIEW.len()..)
                .and_then(|id| <[u8; 4]>::try_from(id).ok())
            {
                leftovers.push(u32::from_be_bytes(id));
            }
        }
        for view_id in leftovers {
            sweep.delete_view_rows(view_id)?;
        }
        Ok(Some(sweep))
    }
}

impl GroupSweep<'_> {
    /// Every view of the group but the sweeping one: `(workspace key, view, view id)`.
    pub(crate) fn other_views(&self) -> Result<Vec<(String, String, u32)>, IndexError> {
        let own = self.db.group.as_ref().map(|own| own.view_id);
        Ok(self
            .group
            .views()?
            .into_iter()
            .filter(|(_, _, id)| Some(*id) != own)
            .collect())
    }

    /// Whether the group owes a sweep of its content rows, or this sweep retired a view.
    pub(crate) fn owes_contents(&self) -> bool {
        self.owed.is_some() || self.report.views_retired > 0
    }

    /// Retire the view `view_id` of `(workspace_key, view)` unless a handle of this process has it
    /// open, an open has claimed it ([`GroupDb::claim_view`]), or its record changed. `true` when it
    /// was retired.
    pub(crate) fn retire_view(&mut self, workspace_key: &str, view: &str, view_id: u32) -> Result<bool, IndexError> {
        {
            let _serial = self.group.allocation.lock().expect("view allocation");
            if self.group.view_id(workspace_key, view)? != Some(view_id)
                || self.group.live_key_views().contains(&view_id)
            {
                return Ok(false);
            }
            let meta = self.group.meta();
            let mut batch = self.group.database().batch().durability(Some(PersistMode::SyncAll));
            batch.remove(meta, view_record(workspace_key, view));
            batch.insert(meta, retired_view_key(view_id), Vec::<u8>::new());
            batch.commit()?;
        }
        self.delete_view_rows(view_id)?;
        self.report.views_retired += 1;
        Ok(true)
    }

    /// Delete every row under `view_id`'s prefix, then its tombstone. The postings its BM25 forward
    /// rows named are owed a sweep from before the first delete.
    fn delete_view_rows(&mut self, view_id: u32) -> Result<(), IndexError> {
        let scoped = |part: Part| Scoped::view(self.group.keyspace(part).clone(), view_id);
        let forward = scoped(Part::CodeBm25ByPath);
        if forward.keys().next().transpose()?.is_some() {
            #[cfg(feature = "code-search")]
            self.db.mark_bm25_sweep_owed()?;
            self.postings_owed = true;
            self.db.remove_view_holders(&forward, view_id)?;
        }
        let db = self.group.database();
        for part in Part::ALL.into_iter().filter(|part| !SHARED_PARTS.contains(part)) {
            scoped(part).clear(db)?;
        }
        self.group.meta().remove(retired_view_key(view_id))?;
        self.group.forget_view(view_id);
        Ok(())
    }

    /// Delete the symbol and call rows of every content `is_live` rejects and no writer has pending.
    pub(crate) fn sweep_contents(&mut self, is_live: impl Fn(&ContentKey) -> bool) -> Result<(), IndexError> {
        let mut dead: Vec<ContentKey> = Vec::new();
        for rows in [&self.db.symbols_by_path, &self.db.calls_by_path] {
            for content in distinct_contents(rows) {
                let content = content?;
                if !is_live(&content) {
                    dead.push(content);
                }
            }
        }
        dead.sort_unstable();
        dead.dedup();
        for batch in dead.chunks(SWEEP_BATCH_CONTENTS) {
            let _exclusive = self.db.bm25_gate.write();
            let pending = self.group.pending();
            let mut writer = self.db.writer();
            let mut removed = 0;
            for content in batch.iter().filter(|content| !pending.holds(content)) {
                writer.remove_content(content)?;
                removed += 1;
            }
            drop(pending);
            writer.commit()?;
            self.report.contents_removed += removed;
        }
        Ok(())
    }

    /// Clear the debt this sweep paid, sweep the postings of retired views, and report.
    pub(crate) fn finish(mut self, contents_swept: bool) -> Result<GroupSweepReport, IndexError> {
        if contents_swept && let Some(owed) = self.owed {
            let _exclusive = self.db.bm25_gate.write();
            if self.db.content_sweep_owed()? == Some(owed) && self.group.pending().is_empty() {
                self.db.meta.raw().remove(META_CONTENT_SWEEP_OWED)?;
            }
        }
        if std::mem::take(&mut self.postings_owed) {
            #[cfg(feature = "code-search")]
            self.db.sweep_owed_bm25_postings()?;
            self.report.postings_swept = cfg!(feature = "code-search");
        }
        Ok(self.report)
    }
}

/// The distinct content keys of a content-led keyspace (`content key ‖ start`), one seek each.
fn distinct_contents(rows: &Scoped) -> impl Iterator<Item = Result<ContentKey, IndexError>> + '_ {
    let mut lower: Option<Vec<u8>> = Some(Vec::new());
    std::iter::from_fn(move || {
        let from = lower.take()?;
        let next = rows
            .range(std::ops::Bound::Included(from.as_slice()), std::ops::Bound::Unbounded)
            .next()?;
        let key = match next {
            Ok((key, _)) => key,
            Err(error) => return Some(Err(error)),
        };
        let content: ContentKey = key.get(..CONTENT_KEY_LEN)?.try_into().ok()?;
        lower = super::name_dict::upper_bound(&content);
        Some(Ok(content))
    })
}

#[cfg(feature = "code-search")]
impl IndexDb {
    /// Sweep the posting list and clear the debt it paid, unless someone owed it again meanwhile.
    fn sweep_owed_bm25_postings(&self) -> Result<(), IndexError> {
        let owed = self.sweep_owed_token()?;
        self.sweep_bm25_postings()?;
        if let Some(owed) = owed {
            self.clear_sweep_owed_if(owed)?;
        }
        Ok(())
    }
}

/// Test access to the debt row.
#[cfg(test)]
impl IndexDb {
    pub(crate) fn content_sweep_owed_for_test(&self) -> bool {
        self.content_sweep_owed().expect("read the debt").is_some()
    }
}

/// Every `(key, value)` row of one keyspace, in key order.
#[cfg(any(feature = "test-support", test))]
pub type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// The rows of a group database, for tests that compare two groups: every row the group shares by
/// content, and the views that hold rows of their own.
#[cfg(any(feature = "test-support", test))]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GroupRows {
    /// Per shared keyspace, by name: every row, in key order.
    pub shared: std::collections::BTreeMap<&'static str, Rows>,
    /// Ids of the views with at least one row under their prefix.
    pub views_with_rows: std::collections::BTreeSet<u32>,
}

#[cfg(any(feature = "test-support", test))]
impl IndexDb {
    /// Every row of this handle's group database, as [`GroupRows`]; empty for a database of its own.
    #[doc(hidden)]
    pub fn group_rows(&self) -> Result<GroupRows, IndexError> {
        let mut rows = GroupRows::default();
        let Some(own) = &self.group else {
            return Ok(rows);
        };
        for part in Part::ALL {
            let keyspace = own.group.keyspace(part);
            if SHARED_PARTS.contains(&part) {
                let mut all = Vec::new();
                for guard in keyspace.iter() {
                    let (key, value) = guard.into_inner()?;
                    all.push((key.to_vec(), value.to_vec()));
                }
                rows.shared.insert(part.name(), all);
                continue;
            }
            for guard in keyspace.iter() {
                let key = guard.key()?;
                // The group's schema stamp predates the `0xFF` group rows. ~keep
                if part == Part::Meta && &*key == super::META_SCHEMA_VER {
                    continue;
                }
                if let Some(prefix) = key.get(..4).and_then(|prefix| <[u8; 4]>::try_from(prefix).ok())
                    && prefix[0] != 0xFF
                {
                    rows.views_with_rows.insert(u32::from_be_bytes(prefix));
                }
            }
        }
        Ok(rows)
    }
}
