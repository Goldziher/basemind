//! How the resolved refinement marks the hits of a name scan.

use super::helpers_calls::{ResolvedCallers, merge_resolved};
use super::helpers_calls_scan::CallScanPage;
use super::types::ReferenceHit;
use crate::path::RelPath;

fn page_of(paths: &[&str]) -> CallScanPage {
    CallScanPage {
        total: paths.len() as u32,
        total_is_partial: false,
        hits: paths
            .iter()
            .map(|path| ReferenceHit {
                path: RelPath::from(*path),
                line: 1,
                column: 0,
                callee: "f".to_string(),
                resolved: None,
            })
            .collect(),
        next_cursor: None,
        stopped_after: None,
        hit_keys: paths.iter().map(|path| path.as_bytes().to_vec()).collect(),
        hit_starts: vec![7; paths.len()],
    }
}

/// A hit is proven, unproven, or in a file whose resolved uses were never probed. The last kind
/// carries no mark: `false` there would claim a check that did not happen.
#[test]
fn a_hit_in_a_file_past_the_probe_cap_stays_unannotated() {
    let mut resolved = ResolvedCallers::default();
    resolved.sites.entry(RelPath::from("proven.rs")).or_default().insert(7);
    resolved.unprobed.insert(RelPath::from("unprobed.rs"));

    let merged = merge_resolved(
        page_of(&["other.rs", "proven.rs", "unprobed.rs"]),
        Some(resolved),
        10,
        None,
    );
    let marks: Vec<Option<bool>> = merged.hits.iter().map(|hit| hit.resolved).collect();
    assert_eq!(marks, [Some(false), Some(true), None]);
}

#[test]
fn without_a_refinement_no_hit_is_marked() {
    let merged = merge_resolved(page_of(&["a.rs"]), None, 10, None);
    assert_eq!(merged.hits[0].resolved, None);
}
