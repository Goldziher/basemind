//! How call sites are graded, and how a page elides, counts and filters the grades.

use super::{UseSources, classify_references, finish_page, most_common};
use crate::config::ConfigV1;
use crate::engine::refs::calls_scan::CallScanPage;
use crate::engine::types::refs::{ReferenceHit, Resolution, ResolutionFilter};
use crate::path::RelPath;
use crate::scanner::{ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};
use serde_json::json;

fn hit(path: &str, line: u32, resolution: Resolution) -> ReferenceHit {
    ReferenceHit {
        path: RelPath::from(path),
        line,
        column: 0,
        callee: "f".to_string(),
        resolution,
        elide_resolution: false,
        shared_line: false,
        stale: false,
    }
}

/// A page of `hits` keyed `[0]`, `[1]`, … in scan order.
fn page(hits: Vec<ReferenceHit>) -> CallScanPage {
    let keys: Vec<Vec<u8>> = (0..hits.len() as u8).map(|at| vec![at]).collect();
    let starts = vec![0; hits.len()];
    CallScanPage::finish(hits.len() as u32, false, hits, keys, starts, false, None)
}

fn rows(hits: &[ReferenceHit]) -> Vec<serde_json::Value> {
    hits.iter().map(|hit| serde_json::to_value(hit).unwrap()).collect()
}

#[test]
fn the_most_common_value_is_the_default_and_a_tie_takes_the_earlier_value() {
    use Resolution::*;
    assert_eq!(most_common([NameOnly, SameFile, NameOnly].into_iter()), Some(NameOnly));
    assert_eq!(most_common([External, Imported].into_iter()), Some(Imported));
    assert_eq!(most_common(std::iter::empty()), None);
}

/// A row writes its resolution only when it is not the page's default, and the counts are left out
/// when every row is the default.
#[test]
fn a_row_equal_to_the_default_leaves_its_resolution_out() {
    let mixed = finish_page(
        page(vec![
            hit("a.rs", 1, Resolution::NameOnly),
            hit("a.rs", 2, Resolution::SameFile),
            hit("b.rs", 3, Resolution::NameOnly),
        ]),
        None,
        "f",
        None,
    );
    assert_eq!(mixed.default, Some(Resolution::NameOnly));
    assert_eq!(
        rows(&mixed.page.hits),
        [
            json!({ "line": 1 }),
            json!({ "line": 2, "resolution": "same_file" }),
            json!({ "line": 3 })
        ]
    );
    assert_eq!(
        serde_json::to_value(mixed.counts).unwrap(),
        json!({ "same_file": 1, "name_only": 2 })
    );

    let uniform = finish_page(
        page(vec![
            hit("a.rs", 1, Resolution::NameOnly),
            hit("b.rs", 2, Resolution::NameOnly),
        ]),
        None,
        "f",
        None,
    );
    assert_eq!(uniform.default, Some(Resolution::NameOnly));
    assert!(uniform.counts.is_none(), "the default alone says it all");

    let empty = finish_page(page(Vec::new()), None, "f", None);
    assert_eq!((empty.default, empty.counts), (None, None));
}

/// The filter drops rows of other values; the counts still say what the page held before it, so a
/// page the filter emptied is told apart from a page with nothing on it.
#[test]
fn the_filter_keeps_the_values_asked_for_and_counts_what_it_dropped() {
    let filter = ResolutionFilter::parse("same_file,imported").unwrap();
    let finished = finish_page(
        page(vec![
            hit("a.rs", 1, Resolution::NameOnly),
            hit("a.rs", 2, Resolution::SameFile),
            hit("b.rs", 3, Resolution::Imported),
            hit("c.rs", 4, Resolution::Imported),
        ]),
        Some(filter),
        "f",
        None,
    );
    assert_eq!(finished.default, Some(Resolution::Imported));
    assert_eq!(
        rows(&finished.page.hits),
        [
            json!({ "line": 2, "resolution": "same_file" }),
            json!({ "line": 3 }),
            json!({ "line": 4 })
        ]
    );
    assert_eq!(
        serde_json::to_value(finished.counts).unwrap(),
        json!({ "same_file": 1, "imported": 2, "name_only": 1 })
    );

    let emptied = finish_page(
        page(vec![hit("a.rs", 1, Resolution::NameOnly)]),
        Some(filter),
        "f",
        None,
    );
    assert!(emptied.page.hits.is_empty());
    assert_eq!(emptied.default, None);
    assert_eq!(serde_json::to_value(emptied.counts).unwrap(), json!({ "name_only": 1 }));
}

/// A budget that cuts the page resumes after the last row kept, so the counts stop there too: the
/// rows past it are counted by the page that lists them.
#[test]
fn counts_of_a_budgeted_page_stop_at_the_last_row_kept() {
    let hits: Vec<ReferenceHit> = (0..40)
        .map(|at| {
            let resolution = if at % 2 == 0 {
                Resolution::NameOnly
            } else {
                Resolution::External
            };
            hit(&format!("file_{at:02}.rs"), at + 1, resolution)
        })
        .collect();
    let finished = finish_page(page(hits), None, "f", Some(40));
    assert!(finished.page.budgeted, "40 tokens hold only a few rows");
    let kept = finished.page.hits.len();
    assert!(kept > 0 && kept < 40, "{kept}");
    let counted: u32 = finished.counts.map_or(kept as u32, |counts| counts.values().sum());
    assert_eq!(counted as usize, kept, "only the rows answered for are counted");
    assert_eq!(
        finished.page.next_cursor.map(|cursor| cursor.0),
        Some(crate::engine::cursor::Cursor::encode_fjall(&[kept as u8 - 1]).0),
        "the cursor resumes after the last row kept"
    );
}

/// Grades against a scanned fixture: a call bound in its file to the function of that name is
/// `same_file` (with oxc), one bound to a parameter is `scope_resolved`, and a member call is not judged by its
/// receiver's binding; a call of a name the index defines nowhere is `external`, and a site in a file
/// of a language with no resolution engine stays `name_only`.
#[test]
fn a_name_scan_grades_each_site_by_what_the_index_knows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("mod.py"),
        b"missing()\nobj = None\nobj.target()\n\n\ndef go(make):\n    return make()\n",
    )
    .expect("mod.py");
    std::fs::write(root.join("util.ts"), b"function target() { return 1; }\ntarget();\n").expect("util.ts");
    std::fs::write(root.join("main.rs"), b"fn main() { target(); missing(); }\n").expect("main.rs");
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
        crate::retrieval_wiring::backend(),
    )
    .expect("scan");
    let cache = crate::engine::MapCache::build(&store, 0);
    let idx = crate::index::view::StandaloneView(store.index_db.clone().expect("a writer store opens the index"));

    let graded = |name: &str| {
        let mut scan = crate::engine::refs::calls_scan::scan_calls_by_name(
            &idx,
            name,
            crate::engine::refs::matching::Matching::Exact,
            100,
            None,
        )
        .expect("scan");
        let mut sources = UseSources::new(crate::engine::files::IndexedFiles::contained(root), &cache);
        classify_references(&idx, &mut sources, &mut scan.hits, &scan.hit_starts);
        let mut graded: Vec<(String, u32, Resolution)> = scan
            .hits
            .iter()
            .map(|hit| (hit.path.to_str_lossy().to_string(), hit.line, hit.resolution))
            .collect();
        graded.sort();
        graded
    };
    // Only oxc binds a top-level function call to its declaration; the `locals` query does not.
    let same_file = if cfg!(feature = "code-intel-js") {
        Resolution::SameFile
    } else {
        Resolution::NameOnly
    };
    assert_eq!(
        graded("target"),
        [
            ("main.rs".to_string(), 1, Resolution::NameOnly),
            ("mod.py".to_string(), 3, Resolution::NameOnly),
            ("util.ts".to_string(), 2, same_file),
        ],
        "line 3 of mod.py is `obj.target()`: `obj` binds, `target` does not"
    );
    assert_eq!(
        graded("missing"),
        [
            ("main.rs".to_string(), 1, Resolution::External),
            ("mod.py".to_string(), 1, Resolution::External)
        ]
    );
    assert_eq!(graded("make"), [("mod.py".to_string(), 7, Resolution::ScopeResolved)]);
}
