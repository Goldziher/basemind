use std::cmp::Ordering;

use super::view::{KvIter, KvResult};

/// Merge encoded keys in ascending order, preferring the overlay on equal keys.
pub(super) fn merge<'a>(
    base: KvIter<'a>,
    overlay: KvIter<'a>,
    mut shadowed: impl FnMut(&[u8]) -> bool + 'a,
) -> KvIter<'a> {
    let mut base = base
        .filter(move |row| match row {
            Ok((key, _)) => !shadowed(key),
            Err(_) => true,
        })
        .peekable();
    let mut overlay = overlay.peekable();

    Box::new(std::iter::from_fn(move || -> Option<KvResult> {
        match (base.peek(), overlay.peek()) {
            (Some(Err(_)), _) | (Some(_), None) => base.next(),
            (_, Some(Err(_))) | (None, Some(_)) => overlay.next(),
            (None, None) => None,
            (Some(Ok((left, _))), Some(Ok((right, _)))) => match left.as_ref().cmp(right.as_ref()) {
                Ordering::Less => base.next(),
                Ordering::Greater => overlay.next(),
                Ordering::Equal => {
                    base.next();
                    overlay.next()
                }
            },
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::view::IndexBytes;

    fn rows(input: &'static [(u8, u8)]) -> KvIter<'static> {
        Box::new(input.iter().map(|&(key, value)| {
            Ok((
                IndexBytes::from_fjall(vec![key].into()),
                IndexBytes::from_fjall(vec![value].into()),
            ))
        }))
    }

    #[test]
    fn merge_orders_rows_and_prefers_overlay_on_collision() {
        let base = rows(&[(1, 10), (2, 20), (4, 40), (6, 60)]);
        let overlay = rows(&[(1, 11), (3, 31), (4, 41), (5, 51)]);
        let merged = merge(base, overlay, |key| key == [2] || key == [4]);
        let actual: Vec<_> = merged
            .map(|row| {
                let (key, value) = row.unwrap();
                (key[0], value[0])
            })
            .collect();
        assert_eq!(actual, [(1, 11), (3, 31), (4, 41), (5, 51), (6, 60)]);
    }
}

#[test]
fn merged_fjall_ranges_match_standalone_across_cursor() {
    use std::ops::Bound::{Excluded, Unbounded};

    use crate::index::view::{IndexView, Part};
    use crate::index::{IndexDb, keys};
    use crate::path::RelPath;

    let directory = tempfile::tempdir().unwrap();
    let base = IndexDb::open(&directory.path().join("base")).unwrap();
    let overlay = IndexDb::open(&directory.path().join("overlay")).unwrap();
    let standalone = IndexDb::open(&directory.path().join("standalone")).unwrap();

    let key = |path| keys::call_by_callee("run", &RelPath::from(path), 1).unwrap();
    let retained = key("alpha.rs");
    let modified = key("bravo.rs");
    let added = key("cello.rs");
    let deleted = key("delta.rs");
    let last = key("ember.rs");

    for (db, rows) in [
        (&base, vec![(&retained, 1), (&modified, 2), (&deleted, 3), (&last, 4)]),
        (&overlay, vec![(&modified, 5), (&added, 6)]),
        (
            &standalone,
            vec![(&retained, 1), (&modified, 5), (&added, 6), (&last, 4)],
        ),
    ] {
        for (key, value) in rows {
            db.calls_by_callee.insert(key.as_slice(), [value]).unwrap();
        }
    }

    let part = Part::CallsByCallee;
    let base = base.view();
    let overlay = overlay.view();
    let standalone = standalone.view();
    let hides = |key: &[u8]| key == deleted.as_slice() || key == modified.as_slice();
    let actual = |after: &[u8]| {
        merge(
            base.range(part, Excluded(after), Unbounded),
            overlay.range(part, Excluded(after), Unbounded),
            hides,
        )
        .map(Result::unwrap)
        .collect::<Vec<_>>()
    };
    let expected = |after: &[u8]| {
        standalone
            .range(part, Excluded(after), Unbounded)
            .map(Result::unwrap)
            .collect::<Vec<_>>()
    };
    assert_eq!(expected(&[]).len(), 4);
    assert_eq!(actual(&[]), expected(&[]));
    assert_eq!(expected(&modified).len(), 2);
    assert_eq!(actual(&modified), expected(&modified));
}
