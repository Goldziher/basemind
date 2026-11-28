//! [`PagedMap::patched`] against a map built from scratch, and the sharing that makes a small patch
//! cheap.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{PAGE, PagedMap, PagedSet};

/// A small deterministic generator, so a failing sequence replays from its seed.
struct Steps(u64);

impl Steps {
    fn next(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below
    }
}

/// Pages are non-empty, in key order across their boundaries, at most twice [`PAGE`] long, and
/// every page but the last holds at least half of it.
fn assert_paged(map: &PagedMap<u64, u64>, label: &str) {
    let pages = &map.pages;
    for (index, page) in pages.iter().enumerate() {
        assert!(!page.is_empty(), "{label}: page {index} is empty");
        assert!(page.len() <= 2 * PAGE, "{label}: page {index} holds {}", page.len());
        if index + 1 < pages.len() {
            assert!(page.len() >= PAGE / 2, "{label}: page {index} holds {}", page.len());
            assert!(
                page[page.len() - 1].0 < pages[index + 1][0].0,
                "{label}: page {index} overlaps"
            );
        }
    }
}

fn entries(map: &PagedMap<u64, u64>) -> Vec<(u64, u64)> {
    map.iter().map(|(key, value)| (*key, *value)).collect()
}

/// Random batches of inserts, replacements and removals, some spread over the whole key space and
/// some packed into one range so pages split and drain: after every batch the patched map holds
/// exactly what a map built from the same entries holds, answers every lookup the same and keeps
/// its pages within bounds.
#[test]
fn a_patched_map_equals_one_built_from_its_entries() {
    for (seed, initial) in [
        (0x9e37_79b9_7f4a_7c15_u64, 0),
        (0x2545_f491_4f6c_dd1d, 1),
        (0xdead_beef, 3 * PAGE + 7),
    ] {
        let mut steps = Steps(seed);
        let mut model: BTreeMap<u64, u64> = (0..initial as u64).map(|key| (key * 4, key)).collect();
        let mut map = PagedMap::from_unsorted(model.iter().map(|(key, value)| (*key, *value)).collect());
        for batch in 0..60 {
            let label = format!("seed {seed:x} batch {batch}");
            let (start, span) = if batch % 3 == 0 {
                (0, 8 * PAGE as u64 * 4)
            } else {
                (steps.next(8 * PAGE as u64 * 4), PAGE as u64)
            };
            let removing = steps.next(3) == 0;
            let mut edits = Vec::new();
            for _ in 0..steps.next(3 * PAGE as u64) {
                let key = start + steps.next(span);
                let value = (!removing || steps.next(4) == 0).then(|| steps.next(1000));
                match value {
                    Some(value) => model.insert(key, value),
                    None => model.remove(&key),
                };
                edits.push((key, value));
            }
            map = map.patched(edits);
            let expected: Vec<(u64, u64)> = model.iter().map(|(key, value)| (*key, *value)).collect();
            assert_eq!(entries(&map), expected, "{label}: entries");
            assert_eq!(map.len(), model.len(), "{label}: len");
            assert_eq!(map, PagedMap::from_unsorted(expected), "{label}: equals a rebuild");
            for _ in 0..64 {
                let key = steps.next(8 * PAGE as u64 * 4 + 16);
                assert_eq!(map.get(&key), model.get(&key), "{label}: get {key}");
            }
            assert_paged(&map, &label);
        }
    }
}

/// A patch of one entry over 100k rebuilds one page and allocates one entry: every other page,
/// and every other entry of the page it lands on, is the same allocation as before.
#[test]
fn a_one_entry_patch_shares_every_other_page_and_entry() {
    let base = PagedMap::from_unsorted((0..100_000u64).map(|key| (key * 2, key)).collect());
    for (label, edit) in [
        ("replace", (50_000, Some(7))),
        ("insert", (50_001, Some(7))),
        ("remove", (50_000, None)),
    ] {
        let next = base.patched([edit]);
        assert_eq!(next.pages.len(), base.pages.len(), "{label}: page count");
        let shared_pages = base
            .pages
            .iter()
            .zip(&next.pages)
            .filter(|(old, new)| Arc::ptr_eq(old, new))
            .count();
        assert_eq!(shared_pages, base.pages.len() - 1, "{label}: shared pages");
        let touched = base.page_of(&edit.0);
        let old: Vec<_> = base.pages[touched].iter().collect();
        let fresh = next.pages[touched]
            .iter()
            .filter(|entry| !old.iter().any(|kept| Arc::ptr_eq(kept, entry)))
            .count();
        assert_eq!(fresh, usize::from(edit.1.is_some()), "{label}: entries allocated");
    }
}

#[test]
fn of_two_edits_of_one_key_the_later_wins() {
    let base = PagedMap::from_unsorted(vec![(1u64, 10u64), (2, 20), (1, 11)]);
    assert_eq!(entries(&base), [(1, 11), (2, 20)]);
    assert_eq!(entries(&base.patched([(1, Some(12)), (1, None)])), [(2, 20)]);
    assert_eq!(entries(&base.patched([(1, None), (1, Some(13))])), [(1, 13), (2, 20)]);
    assert_eq!(entries(&base.patched([(3, None)])), [(1, 11), (2, 20)]);
    assert_eq!(
        entries(&PagedMap::default().patched([(5, Some(50)), (4, None)])),
        [(5, 50)]
    );
}

#[test]
fn a_patched_set_holds_what_its_edits_leave() {
    let set = PagedSet::from_unsorted(vec!["b", "a", "d"]);
    let next = set.patched([("c", true), ("a", false), ("e", false)]);
    assert_eq!(next.iter().copied().collect::<Vec<_>>(), ["b", "c", "d"]);
    assert_eq!(next.len(), 3);
    assert!(next.contains(&"c"));
    assert!(!next.contains(&"a"));
    assert_eq!(next, PagedSet::from_unsorted(vec!["d", "c", "b"]));
    assert_eq!(
        set.iter().copied().collect::<Vec<_>>(),
        ["a", "b", "d"],
        "the base is unchanged"
    );
}
