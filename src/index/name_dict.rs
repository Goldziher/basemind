//! Resident dictionary of the distinct leading names (callee / trait) in a length-prefixed keyspace.
//!
//! `calls_by_callee` and `implementations_by_trait` are keyed `u16:len(name) ‖ name ‖ …`, and the
//! `references` / `callers` / `implementations` modes match the name by SUBSTRING, so a prefix scan
//! cannot serve them and the old shape walked every key in the partition. The distinct names are
//! orders of magnitude fewer than the keys (a call site per key, many sites per name). This keeps
//! those names in one contiguous allocation: a substring query is a `memmem` sweep over it that
//! yields the matching names in KEY order (`len`, then bytes), and the caller then range-scans only
//! those names' key ranges. The emitted keys, their order, totals and cursors are exactly what the
//! full walk produces.
//!
//! The dictionary is a SUPERSET of the names that have keys: the writer records a name before it
//! stages the key, and nothing is ever removed until the process restarts. A stale name costs one
//! empty range scan; a missing name would lose results, which the record-before-write order and the
//! build protocol below rule out.
//!
//! Build: the first query spawns a background pass that collects every distinct name (names recorded
//! while it runs are merged in on completion) and answers that query with the legacy walk. Fjall
//! admits a single holder per directory, so the process owning the [`IndexDb`](super::IndexDb) is
//! the only writer and sees every name.

use std::collections::HashSet;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard};

use fjall::Keyspace;

/// Terminates every name in the blob. `0xFF` never occurs in UTF-8, so neither a name nor a needle
/// can contain it and a sweep cannot match across two names.
const SEP: u8 = 0xFF;

/// Names recorded since the last fold past which the next query folds them into the snapshot.
const FOLD_EXTRA_AT: usize = 16_384;

/// Consecutive keys sharing one name after which the build seeks past that name instead of
/// stepping through its remaining keys.
const SEEK_AFTER_RUN: usize = 8;

/// A needle matching more than this percentage of the distinct names is answered by the full walk:
/// each matching name costs a seek, while a walk over a near-universal needle reaches the result
/// cap after a handful of keys.
const WALK_ABOVE_PERCENT: usize = 25;

/// Order of a name's key range within the keyspace: the `u16` length prefix, then the bytes.
fn key_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Distinct names in key order, as one blob of `name ‖ SEP` records.
#[derive(Default)]
struct Snapshot {
    blob: Vec<u8>,
    /// Start offset of each name, plus one trailing entry at `blob.len()`.
    starts: Vec<u32>,
}

impl Snapshot {
    /// `names` must already be in key order and distinct.
    fn from_sorted<'a>(names: impl Iterator<Item = &'a [u8]>) -> Self {
        let mut blob = Vec::new();
        let mut starts = Vec::new();
        for name in names {
            starts.push(blob.len() as u32);
            blob.extend_from_slice(name);
            blob.push(SEP);
        }
        starts.push(blob.len() as u32);
        blob.shrink_to_fit();
        starts.shrink_to_fit();
        Self { blob, starts }
    }

    fn len(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    fn name(&self, index: usize) -> &[u8] {
        &self.blob[self.starts[index] as usize..self.starts[index + 1] as usize - 1]
    }

    fn contains(&self, name: &[u8]) -> bool {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match key_order(self.name(mid), name) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    /// Names containing the needle, in key order.
    fn matching<'a>(&'a self, finder: &memchr::memmem::Finder<'_>) -> Vec<&'a [u8]> {
        if finder.needle().is_empty() {
            return (0..self.len()).map(|i| self.name(i)).collect();
        }
        let mut out = Vec::new();
        let mut from = 0usize;
        while from < self.blob.len() {
            let Some(found) = finder.find(&self.blob[from..]) else {
                break;
            };
            let pos = from + found;
            // The name holding `pos`: the last one starting at or before it.
            let index = self.starts.partition_point(|&start| (start as usize) <= pos) - 1;
            out.push(self.name(index));
            from = self.starts[index + 1] as usize;
        }
        out
    }
}

#[derive(Default)]
struct State {
    snapshot: Option<Arc<Snapshot>>,
    /// Names recorded but not yet in `snapshot`.
    extra: HashSet<String>,
    building: bool,
    /// The build failed; every query keeps using the full walk.
    disabled: bool,
}

/// See the module docs.
pub struct NameDict {
    state: Mutex<State>,
    /// [`WALK_ABOVE_PERCENT`], adjustable so tests can force the dictionary path.
    walk_above_percent: std::sync::atomic::AtomicUsize,
}

impl Default for NameDict {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            walk_above_percent: std::sync::atomic::AtomicUsize::new(WALK_ABOVE_PERCENT),
        }
    }
}

impl NameDict {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record names about to be written. Must run BEFORE the keys are staged.
    pub(crate) fn record<'a>(&self, names: impl Iterator<Item = &'a str>) {
        let mut state = self.lock();
        for name in names {
            if name.len() > usize::from(u16::MAX) {
                continue;
            }
            if state.snapshot.as_ref().is_some_and(|s| s.contains(name.as_bytes())) || state.extra.contains(name) {
                continue;
            }
            state.extra.insert(name.to_owned());
        }
    }

    /// Key prefixes (`u16:len ‖ name`) of every name containing `needle`, in key order. `None`
    /// while the dictionary is not built: the first call starts the build and the caller walks the
    /// whole keyspace as before.
    pub(crate) fn matching_prefixes(self: &Arc<Self>, keyspace: &Keyspace, needle: &str) -> Option<Vec<Vec<u8>>> {
        let total;
        let (snapshot, extra) = {
            let mut state = self.lock();
            if state.disabled {
                return None;
            }
            if state.snapshot.is_none() {
                if !state.building {
                    state.building = true;
                    let (dict, keyspace) = (Arc::clone(self), keyspace.clone());
                    let spawned = std::thread::Builder::new()
                        .name("name-dict-build".into())
                        .spawn(move || dict.build(&keyspace));
                    if spawned.is_err() {
                        state.building = false;
                        state.disabled = true;
                    }
                }
                return None;
            }
            if state.extra.len() > FOLD_EXTRA_AT {
                Self::fold(&mut state);
            }
            let snapshot = Arc::clone(state.snapshot.as_ref()?);
            let extra: Vec<String> = state.extra.iter().filter(|n| n.contains(needle)).cloned().collect();
            total = snapshot.len() + state.extra.len();
            (snapshot, extra)
        };
        let finder = memchr::memmem::Finder::new(needle.as_bytes());
        let mut names: Vec<&[u8]> = snapshot.matching(&finder);
        let known = names.len();
        names.extend(extra.iter().map(String::as_bytes));
        if names.len() > known {
            names.sort_unstable_by(|a, b| key_order(a, b));
            names.dedup();
        }
        let percent = self.walk_above_percent.load(std::sync::atomic::Ordering::Relaxed);
        if names.len().saturating_mul(100) > total.saturating_mul(percent) {
            return None;
        }
        Some(
            names
                .into_iter()
                .map(|name| {
                    let mut prefix = Vec::with_capacity(2 + name.len());
                    prefix.extend_from_slice(&(name.len() as u16).to_be_bytes());
                    prefix.extend_from_slice(name);
                    prefix
                })
                .collect(),
        )
    }

    /// Merge `extra` into the snapshot.
    fn fold(state: &mut State) {
        let snapshot = state.snapshot.clone();
        let mut merged: Vec<&[u8]> = Vec::new();
        if let Some(snapshot) = &snapshot {
            merged.extend((0..snapshot.len()).map(|i| snapshot.name(i)));
        }
        merged.extend(state.extra.iter().map(String::as_bytes));
        merged.sort_unstable_by(|a, b| key_order(a, b));
        merged.dedup();
        let folded = Snapshot::from_sorted(merged.into_iter());
        state.snapshot = Some(Arc::new(folded));
        state.extra.clear();
    }

    /// Collect every distinct name, then publish. Runs on the build thread.
    fn build(&self, keyspace: &Keyspace) {
        let result = distinct_names(keyspace);
        let mut state = self.lock();
        state.building = false;
        match result {
            Ok(names) => {
                state.snapshot = Some(Arc::new(Snapshot::from_sorted(names.iter().map(Vec::as_slice))));
                // Names recorded while the pass ran may postdate its view of the keyspace.
                Self::fold(&mut state);
            }
            Err(error) => {
                tracing::warn!(%error, "name dictionary build failed; falling back to full keyspace walks");
                state.disabled = true;
            }
        }
    }

    /// Build synchronously (tests and benchmarks).
    #[cfg(test)]
    pub(crate) fn build_blocking(&self, keyspace: &Keyspace) {
        self.lock().building = true;
        self.build(keyspace);
    }

    /// Answer from the dictionary however many names match.
    #[cfg(test)]
    pub(crate) fn always_use_dictionary(&self) {
        self.walk_above_percent
            .store(usize::MAX / 1_000, std::sync::atomic::Ordering::Relaxed);
    }

    /// Force every query onto the full-partition walk (the reference the dictionary must match).
    #[cfg(test)]
    pub(crate) fn set_disabled(&self, disabled: bool) {
        self.lock().disabled = disabled;
    }

    /// Distinct names currently held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        let state = self.lock();
        state.snapshot.as_ref().map_or(0, |s| s.len()) + state.extra.len()
    }

    /// Resident bytes of the built snapshot.
    #[cfg(test)]
    pub(crate) fn snapshot_bytes(&self) -> usize {
        self.lock()
            .snapshot
            .as_ref()
            .map_or(0, |s| s.blob.len() + s.starts.len() * 4)
    }
}

/// Every distinct leading name of `keyspace`, in key order. Keys sharing a name are adjacent, so a
/// long run is skipped with one seek past the name's range instead of stepping through it.
fn distinct_names(keyspace: &Keyspace) -> Result<Vec<Vec<u8>>, fjall::Error> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut lower: Bound<Vec<u8>> = Bound::Unbounded;
    'seek: loop {
        let mut run = 0usize;
        for guard in keyspace.range::<Vec<u8>, _>((lower.clone(), Bound::Unbounded)) {
            let (key, _) = guard.into_inner()?;
            let Some(prefix_len) = key.get(..2).map(|b| 2 + usize::from(u16::from_be_bytes([b[0], b[1]]))) else {
                continue;
            };
            let Some(prefix) = key.get(..prefix_len) else {
                continue;
            };
            let same = names
                .last()
                .is_some_and(|last| last.len() + 2 == prefix_len && last[..] == prefix[2..]);
            if same {
                run += 1;
                if run >= SEEK_AFTER_RUN {
                    match upper_bound(prefix) {
                        Some(next) => {
                            lower = Bound::Included(next);
                            continue 'seek;
                        }
                        None => break 'seek,
                    }
                }
            } else {
                if std::str::from_utf8(&prefix[2..]).is_ok() {
                    names.push(prefix[2..].to_vec());
                }
                run = 1;
            }
        }
        break;
    }
    Ok(names)
}

/// Smallest key greater than every key starting with `prefix`; `None` when there is none.
pub(crate) fn upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == 0xFF {
            out.pop();
            continue;
        }
        *last += 1;
        return Some(out);
    }
    None
}

/// The keys of `keyspace` whose leading name is one of `prefixes` (key-ordered, as returned by
/// [`NameDict::matching_prefixes`]), strictly after `cursor_after`, in key order.
pub(crate) fn scan_prefixes(
    keyspace: &Keyspace,
    prefixes: Vec<Vec<u8>>,
    cursor_after: Option<Vec<u8>>,
) -> impl Iterator<Item = fjall::Guard> + '_ {
    prefixes.into_iter().flat_map(move |prefix| {
        let lower = match &cursor_after {
            // The cursor lies beyond this name's whole range.
            Some(cursor) if cursor.as_slice() > prefix.as_slice() && !cursor.starts_with(&prefix) => None,
            Some(cursor) if cursor.starts_with(&prefix) => Some(Bound::Excluded(cursor.clone())),
            _ => Some(Bound::Included(prefix.clone())),
        };
        let upper = match upper_bound(&prefix) {
            Some(bound) => Bound::Excluded(bound),
            None => Bound::Unbounded,
        };
        lower
            .map(|lower| keyspace.range::<Vec<u8>, _>((lower, upper)))
            .into_iter()
            .flatten()
    })
}

/// Keys of `keyspace` after `lower`, in key order, whose leading name contains `needle`, plus
/// possibly other keys (callers still filter by name). Served from the dictionary when it is built,
/// otherwise the whole partition from `lower` while the dictionary builds in the background.
pub(crate) fn name_ordered_keys<'a>(
    keyspace: &'a Keyspace,
    dict: &Arc<NameDict>,
    needle: &str,
    cursor_after: Option<&[u8]>,
    lower: Bound<Vec<u8>>,
) -> Box<dyn Iterator<Item = fjall::Guard> + 'a> {
    match dict.matching_prefixes(keyspace, needle) {
        Some(prefixes) => Box::new(scan_prefixes(keyspace, prefixes, cursor_after.map(<[u8]>::to_vec))),
        None => Box::new(keyspace.range::<Vec<u8>, _>((lower, Bound::Unbounded))),
    }
}
