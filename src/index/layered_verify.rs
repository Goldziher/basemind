use anyhow::{Result, ensure};

use super::view::{IndexView, Part};

/// Verify the persisted row contract before publishing an overlay for readers.
pub(super) fn verify_rows(layered: &dyn IndexView, standalone: &dyn IndexView) -> Result<()> {
    let mut visited = 0;
    for part in Part::ALL.into_iter().filter(|part| *part != Part::Meta) {
        let mut layered = layered.iter(part);
        let mut standalone = standalone.iter(part);
        let mut row = 0;
        loop {
            let left = layered.next().transpose()?;
            let right = standalone.next().transpose()?;
            ensure!(left == right, "overlay mismatch in {} at row {row}", part.name());
            if left.is_none() {
                break;
            }
            row += 1;
        }
        visited += row;
    }
    ensure!(visited > 0, "verification compared no index rows");
    ensure!(
        layered.bm25_stats() == standalone.bm25_stats(),
        "overlay BM25 totals differ"
    );
    ensure!(
        layered.import_probes_complete() == standalone.import_probes_complete(),
        "overlay import probe completeness differs"
    );
    Ok(())
}
