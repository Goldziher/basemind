//! How the resolved refinement grades the hits of a name scan.

use super::{ResolvedCallers, merge_resolved};
use crate::engine::refs::calls_scan::CallScanPage;
use crate::engine::types::refs::{ReferenceHit, Resolution};
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
                resolution: Resolution::NameOnly,
                elide_resolution: false,
                shared_line: false,
                stale: false,
                ambiguous: false,
            })
            .collect(),
        next_cursor: None,
        stopped_after: None,
        hit_keys: paths.iter().map(|path| path.as_bytes().to_vec()).collect(),
        hit_starts: vec![7; paths.len()],
    }
}

/// A hit is proven, checked and not proven, or in a file whose resolved uses were never probed. The
/// last stays `name_only`: `elsewhere` there would claim a check that did not happen.
#[test]
fn a_hit_in_a_file_past_the_probe_cap_stays_name_only() {
    let mut resolved = ResolvedCallers::default();
    resolved.sites.entry(RelPath::from("proven.rs")).or_default().insert(7);
    resolved.sites.entry(RelPath::from("def.rs")).or_default().insert(7);
    resolved.unprobed.insert(RelPath::from("unprobed.rs"));

    let merged = merge_resolved(
        page_of(&["def.rs", "other.rs", "proven.rs", "unprobed.rs"]),
        Some(resolved),
        &RelPath::from("def.rs"),
        10,
        None,
    );
    let marks: Vec<Resolution> = merged.hits.iter().map(|hit| hit.resolution).collect();
    assert_eq!(
        marks,
        [
            Resolution::SameFile,
            Resolution::Elsewhere,
            Resolution::Imported,
            Resolution::NameOnly
        ]
    );
}

#[test]
fn without_a_refinement_every_hit_stays_name_only() {
    let merged = merge_resolved(page_of(&["a.rs"]), None, &RelPath::from("a.rs"), 10, None);
    assert_eq!(merged.hits[0].resolution, Resolution::NameOnly);
}
