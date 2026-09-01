//! The scan pipeline's one chunked-drive primitive: a serial loop over weight-bounded chunks of
//! work whose interior runs in parallel.
//!
//! ## The invariant this enforces
//!
//! > No structure whose size is O(total files in the repo) may be simultaneously fully
//! > materialised and live across a phase boundary.
//!
//! Every phase that maps each file to a per-file result — the primary scan's `FileResult`s, the
//! resolve pass's `FileResolvedRefs` — hits its peak by materialising the whole corpus's results
//! and only then consuming them. This driver makes that shape unrepresentable for its callers: it
//! owns the `Vec<R>` a chunk produced, hands it to `absorb` *by value*, and drops it before the
//! next chunk is computed. Live per-item results are therefore O(chunk), never O(corpus).
//!
//! `process` and `absorb` are deliberately two closures rather than one. Fusing them would let a
//! caller keep every chunk's results in an accumulator of its own and the bound would evaporate;
//! split, it is the *driver* that decides when a chunk's results die. A caller that genuinely
//! needs something to outlive its chunk must project it into a different — and, being a
//! projection, smaller — type inside `absorb`. That projection is the whole point: the retained
//! type should have no field capable of holding the O(corpus) part.
//!
//! ## Why a weight budget and not just a count
//!
//! A count says nothing about bytes. One machine-generated source file can carry more resolved
//! edges (or symbols, or postings) than a thousand hand-written ones, so a pure count bound is the
//! same unbounded multiplier with a different constant. Each chunk is therefore cut on *either*
//! bound: whichever trips first. An item heavier than the whole budget still forms a chunk of one
//! — the driver never yields an empty chunk, so progress is guaranteed.
//!
//! ## No channels, no tokio
//!
//! The driver is a plain serial loop; all parallelism lives inside the caller's `process` closure
//! (rayon), which satisfies the scanner pipeline's rayon-only rule — see
//! `.ai-rulez/context/scanner-pipeline.md`.

/// Where to cut one chunk: a count bound and a weight bound, applied together.
///
/// `max_weight` is in whatever unit the caller's `weigh` function returns — source bytes for the
/// resolve pass. It is a *budget*, not a limit: the chunk that crosses it includes the item that
/// did, because a chunk must never be empty.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkCut {
    max_items: usize,
    max_weight: u64,
}

impl ChunkCut {
    /// Both bounds must be non-zero; a zero bound would cut every chunk to nothing and the driver
    /// clamps it to one item rather than spinning.
    pub(crate) const fn new(max_items: usize, max_weight: u64) -> Self {
        Self { max_items, max_weight }
    }

    /// Index one past the last item of the chunk starting at `start`. Always `> start` while
    /// items remain.
    fn boundary<T>(&self, items: &[T], start: usize, weigh: &impl Fn(&T) -> u64) -> usize {
        let mut weight = 0u64;
        for (offset, item) in items[start..].iter().enumerate() {
            if offset > 0 && offset >= self.max_items {
                return start + offset;
            }
            weight = weight.saturating_add(weigh(item));
            if weight >= self.max_weight {
                return start + offset + 1;
            }
        }
        items.len()
    }
}

/// Drive `items` through `process` in chunks cut by `cut`, handing each chunk's results to
/// `absorb` and dropping them before the next chunk begins.
///
/// `process` is the parallel interior (rayon inside); `absorb` is serial and is the only place a
/// chunk's results are visible. See the module docs for why the split is load-bearing.
pub(crate) fn drive_chunks<T, R>(
    items: &[T],
    cut: ChunkCut,
    weigh: impl Fn(&T) -> u64,
    mut process: impl FnMut(&[T]) -> Vec<R>,
    mut absorb: impl FnMut(Vec<R>),
) {
    drive_chunks_governed(
        &mut (),
        items,
        || Some(cut),
        |(), item| weigh(item),
        |(), chunk| process(chunk),
        |(), produced| absorb(produced),
    );
}

/// [`drive_chunks`] with the two things the primary scan needs and the resolve pass does not:
/// a caller state threaded *through* the driver, and a cut re-decided before every chunk.
///
/// **Why `ctx` instead of captured state.** The scan's `process` needs `&Store` (rayon workers read
/// blobs and the index through it) while its `absorb` needs `&mut Store` (it drains each result's
/// staged `FileEntry` into the file map). Two closures cannot capture those two borrows at once, so
/// the driver owns the state and lends each half the borrow it needs. `C = ()` recovers the plain
/// [`drive_chunks`] shape.
///
/// **Why `next_cut` is a closure.** It is the drive loop's admission point: it samples the memory
/// gate, narrows the cut when the process is over its ceiling, and returns `None` to stop the drive
/// entirely (cancellation). Deciding the cut per chunk is what makes the bound *actuating* rather
/// than merely advisory — see [`crate::scanner_drive`].
pub(crate) fn drive_chunks_governed<C, T, R>(
    ctx: &mut C,
    items: &[T],
    mut next_cut: impl FnMut() -> Option<ChunkCut>,
    weigh: impl Fn(&C, &T) -> u64,
    mut process: impl FnMut(&C, &[T]) -> Vec<R>,
    mut absorb: impl FnMut(&mut C, Vec<R>),
) {
    let mut start = 0usize;
    while start < items.len() {
        let Some(cut) = next_cut() else {
            return;
        };
        let end = {
            let ctx: &C = ctx;
            cut.boundary(items, start, &|item: &T| weigh(ctx, item))
        };
        let produced = process(ctx, &items[start..end]);
        absorb(ctx, produced);
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Run the driver, returning the length of every chunk it cut.
    fn chunk_lengths<T>(items: &[T], cut: ChunkCut, weigh: impl Fn(&T) -> u64) -> Vec<usize> {
        let mut lengths = Vec::new();
        drive_chunks(
            items,
            cut,
            weigh,
            |chunk| {
                lengths.push(chunk.len());
                Vec::<()>::new()
            },
            |_| {},
        );
        lengths
    }

    /// The defect the weight bound exists for: a handful of edge-heavy items must force a chunk
    /// boundary that the count bound would sail straight past.
    #[test]
    fn the_weight_budget_cuts_chunks_the_count_bound_would_not() {
        let heavy: Vec<u64> = vec![4 * 1024 * 1024; 8];
        let cut = ChunkCut::new(1024, 8 * 1024 * 1024);

        let lengths = chunk_lengths(&heavy, cut, |bytes| *bytes);

        assert_eq!(
            lengths,
            vec![2, 2, 2, 2],
            "eight 4 MiB items under an 8 MiB budget must cut every two items, not once at 1024"
        );
    }

    /// The count bound still governs the common case, where per-item weight is negligible.
    #[test]
    fn the_count_bound_governs_when_items_are_light() {
        let light: Vec<u64> = vec![16; 10];
        let lengths = chunk_lengths(&light, ChunkCut::new(4, 8 * 1024 * 1024), |bytes| *bytes);
        assert_eq!(lengths, vec![4, 4, 2]);
    }

    /// A single item heavier than the entire budget must still make progress rather than
    /// producing an empty chunk forever.
    #[test]
    fn an_item_heavier_than_the_budget_forms_a_chunk_of_one() {
        let items: Vec<u64> = vec![u64::MAX, 1, 1];
        let lengths = chunk_lengths(&items, ChunkCut::new(1024, 1024), |bytes| *bytes);
        assert_eq!(lengths, vec![1, 2]);
    }

    /// Chunking is a scheduling change only: every item is processed exactly once, in order.
    #[test]
    fn every_item_is_processed_exactly_once_in_order() {
        let items: Vec<u32> = (0..37).collect();
        let mut seen: Vec<u32> = Vec::new();
        drive_chunks(
            &items,
            ChunkCut::new(5, 64),
            |_| 8,
            |chunk| chunk.to_vec(),
            |produced| seen.extend(produced),
        );
        assert_eq!(seen, items);
    }

    /// The governed driver re-asks for a cut before every chunk, so a caller that narrows under
    /// pressure narrows the very next chunk rather than the next scan.
    #[test]
    fn a_governed_drive_re_cuts_before_every_chunk() {
        let items: Vec<u32> = (0..15).collect();
        let mut budget = 8usize;
        let mut lengths: Vec<usize> = Vec::new();
        drive_chunks_governed(
            &mut lengths,
            &items,
            || {
                let cut = ChunkCut::new(budget, u64::MAX);
                budget = (budget / 2).max(1);
                Some(cut)
            },
            |_, _| 0,
            |_, chunk| vec![chunk.len()],
            |lengths: &mut Vec<usize>, produced| lengths.extend(produced),
        );
        assert_eq!(lengths, vec![8, 4, 2, 1]);
    }

    /// `None` from the cut stops the drive where it stands — the scan's cancellation seam.
    #[test]
    fn a_governed_drive_stops_when_the_cut_is_withdrawn() {
        let items: Vec<u32> = (0..100).collect();
        let mut driven = 0usize;
        let mut cuts = 0usize;
        drive_chunks_governed(
            &mut driven,
            &items,
            || {
                cuts += 1;
                (cuts <= 2).then(|| ChunkCut::new(5, u64::MAX))
            },
            |_, _| 0,
            |_, chunk| vec![chunk.len()],
            |driven: &mut usize, produced| *driven += produced.iter().sum::<usize>(),
        );
        assert_eq!(driven, 10, "the drive must stop at the first withdrawn cut");
    }

    /// Counts live instances of itself so a test can observe the driver's real peak.
    struct LiveProbe;

    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    impl LiveProbe {
        fn new() -> Self {
            let live = LIVE.fetch_add(1, Ordering::Relaxed) + 1;
            PEAK.fetch_max(live, Ordering::Relaxed);
            Self
        }
    }

    impl Drop for LiveProbe {
        fn drop(&mut self) {
            LIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// The invariant itself: a chunk's results are never live past that chunk, so the peak live
    /// set is O(chunk) even when the item count is orders of magnitude larger.
    #[test]
    fn no_chunk_result_is_live_past_its_own_chunk() {
        LIVE.store(0, Ordering::Relaxed);
        PEAK.store(0, Ordering::Relaxed);
        let items: Vec<u32> = (0..512).collect();
        let cut = ChunkCut::new(16, u64::MAX);

        drive_chunks(
            &items,
            cut,
            |_| 1,
            |chunk| chunk.iter().map(|_| LiveProbe::new()).collect(),
            |produced| {
                assert_eq!(
                    LIVE.load(Ordering::Relaxed),
                    produced.len(),
                    "absorb must see exactly its own chunk's results and no earlier chunk's"
                );
            },
        );

        assert_eq!(
            LIVE.load(Ordering::Relaxed),
            0,
            "the last chunk's results must be dropped too"
        );
        assert_eq!(
            PEAK.load(Ordering::Relaxed),
            16,
            "peak live results must be the chunk size, not the 512 items driven"
        );
    }
}
