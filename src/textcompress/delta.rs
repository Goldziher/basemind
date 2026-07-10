//! Delta re-reads — the stateless line-diff primitive (token-reduction
//! workstream W6, slice 3).
//!
//! When an agent re-reads a file it already saw, emitting only the CHANGED
//! lines (a `+N/-M` diff) instead of the full content saves most of the tokens.
//! This module is the pure, stateless diff PRIMITIVE: given the OLD and NEW
//! content it produces a compact unified-ish diff plus the change counts. The
//! stateful read-cache hook that decides WHEN to call it is a later slice.
//!
//! Algorithm: a standard LCS (longest common subsequence) table walk over
//! LINES. Lines present in both sides are omitted; deletions are `-`-prefixed,
//! insertions `+`-prefixed, and a `+A/-R` summary header leads the body.
//!
//! Bail guard: LCS is O(n*m) in time and space, so on oversize inputs (either
//! side over [`MAX_DELTA_BYTES`] or [`MAX_DELTA_LINES`]) we never build the
//! table — we return [`DeltaOutcome::bailed`] carrying the NEW content verbatim
//! behind a marker line so the caller can fall back to a full re-read.
//!
//! Ported in spirit from the alexgreensh/token-optimizer `delta-diff.ts`
//! reference, adapted to a pure `(&str, &str) -> DeltaOutcome` Rust transform
//! with zero new dependencies (the LCS is hand-rolled).

/// Inputs at or above this byte length (on either side) skip LCS and bail to a
/// full re-read. LCS is O(n*m) in space; this bounds the table.
pub const MAX_DELTA_BYTES: usize = 50_000;

/// Inputs at or above this line count (on either side) skip LCS and bail.
pub const MAX_DELTA_LINES: usize = 2_000;

/// Marker emitted when the inputs are identical. The caller can serve a digest.
const UNCHANGED_MARKER: &str = "# unchanged";

/// Marker leading the bail output; the NEW content follows verbatim below it.
const BAIL_MARKER: &str = "# file too large for delta; full content follows";

/// Result of a [`delta`] call.
///
/// `output` is the text to surface: the compact diff when `changed && !bailed`,
/// the `UNCHANGED_MARKER` when `!changed`, or the `BAIL_MARKER` followed by
/// the NEW content verbatim when `bailed`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct DeltaOutcome {
    /// The text to emit downstream (diff, unchanged marker, or bail + content).
    pub output: String,
    /// Line count of the OLD side (`old.lines().count()`).
    pub old_lines: usize,
    /// Line count of the NEW side (`new.lines().count()`).
    pub new_lines: usize,
    /// Number of inserted (NEW-only) lines emitted as `+` ops.
    pub added: usize,
    /// Number of deleted (OLD-only) lines emitted as `-` ops.
    pub removed: usize,
    /// Whether the two sides differ at all. `false` ⇒ `UNCHANGED_MARKER`.
    pub changed: bool,
    /// Whether the bail guard fired (oversize input). `output` then carries the
    /// NEW content verbatim behind `BAIL_MARKER`; `added` / `removed` are 0.
    pub bailed: bool,
}

/// Compute a compact line-diff from `old` to `new`.
///
/// - Identical inputs ⇒ `changed = false`, `output = "# unchanged"`.
/// - Either side over [`MAX_DELTA_BYTES`] / [`MAX_DELTA_LINES`] ⇒ `bailed =
///   true`, `output = "# file too large for delta; full content follows\n<new>"`.
/// - Otherwise an LCS walk ⇒ a `+A/-R` header, then `-`/`+` lines for the
///   changed regions; common lines are omitted.
pub fn delta(old: &str, new: &str) -> DeltaOutcome {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let old_line_count = old_lines.len();
    let new_line_count = new_lines.len();

    if old == new {
        return DeltaOutcome {
            output: UNCHANGED_MARKER.to_string(),
            old_lines: old_line_count,
            new_lines: new_line_count,
            added: 0,
            removed: 0,
            changed: false,
            bailed: false,
        };
    }

    if old.len() > MAX_DELTA_BYTES
        || new.len() > MAX_DELTA_BYTES
        || old_line_count > MAX_DELTA_LINES
        || new_line_count > MAX_DELTA_LINES
    {
        return DeltaOutcome {
            output: format!("{BAIL_MARKER}\n{new}"),
            old_lines: old_line_count,
            new_lines: new_line_count,
            added: 0,
            removed: 0,
            changed: true,
            bailed: true,
        };
    }

    let ops = lcs_diff(&old_lines, &new_lines);
    let added = ops.iter().filter(|op| matches!(op, DiffOp::Add(_))).count();
    let removed = ops.iter().filter(|op| matches!(op, DiffOp::Del(_))).count();

    let mut output = format!("+{added}/-{removed}");
    for op in &ops {
        match op {
            DiffOp::Add(line) => {
                output.push_str("\n+");
                output.push_str(line);
            }
            DiffOp::Del(line) => {
                output.push_str("\n-");
                output.push_str(line);
            }
        }
    }

    DeltaOutcome {
        output,
        old_lines: old_line_count,
        new_lines: new_line_count,
        added,
        removed,
        changed: true,
        bailed: false,
    }
}

/// A single diff operation. Common (unchanged) lines are omitted entirely, so
/// only insertions and deletions are represented.
enum DiffOp<'a> {
    /// A line present only in the NEW side (`+`-prefixed on output).
    Add(&'a str),
    /// A line present only in the OLD side (`-`-prefixed on output).
    Del(&'a str),
}

/// Standard LCS dynamic-programming diff over lines.
///
/// Builds the `(n+1) * (m+1)` LCS-length table bottom-up, then walks it
/// top-down emitting `Del` / `Add` ops for the lines outside the common
/// subsequence. Common lines are skipped (omitted from the compact output).
/// Callers MUST gate inputs through the bail guard in [`delta`] first — this is
/// O(n*m) in both time and space.
fn lcs_diff<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffOp<'a>> {
    let n = a.len();
    let m = b.len();
    let stride = m + 1;
    let mut lcs = vec![0u32; (n + 1) * stride];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * stride + j] = if a[i] == b[j] {
                lcs[(i + 1) * stride + (j + 1)] + 1
            } else {
                lcs[(i + 1) * stride + j].max(lcs[i * stride + (j + 1)])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * stride + j] >= lcs[i * stride + (j + 1)] {
            ops.push(DiffOp::Del(a[i]));
            i += 1;
        } else {
            ops.push(DiffOp::Add(b[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffOp::Del(a[i]));
        i += 1;
    }
    while j < m {
        ops.push(DiffOp::Add(b[j]));
        j += 1;
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_report_unchanged() {
        let text = "alpha\nbeta\ngamma\n";
        let r = delta(text, text);
        assert!(!r.changed, "identical inputs must not be changed");
        assert!(!r.bailed);
        assert_eq!(r.added, 0);
        assert_eq!(r.removed, 0);
        assert_eq!(r.output, "# unchanged");
        assert_eq!(r.old_lines, 3);
        assert_eq!(r.new_lines, 3);
    }

    #[test]
    fn changed_lines_emit_correct_counts_and_only_changed_lines() {
        let old = "alpha\nbeta\ngamma\ndelta\n";
        let new = "alpha\nbeta2\ngamma\ndelta\nepsilon\n";
        let r = delta(old, new);

        assert!(r.changed);
        assert!(!r.bailed);
        assert_eq!(r.added, 2, "beta2 + epsilon are the two adds");
        assert_eq!(r.removed, 1, "beta is the single deletion");

        let mut lines = r.output.lines();
        assert_eq!(lines.next(), Some("+2/-1"));

        assert!(r.output.contains("-beta"));
        assert!(r.output.contains("+beta2"));
        assert!(r.output.contains("+epsilon"));
        assert!(
            !r.output.contains("alpha"),
            "common line alpha must be omitted: {}",
            r.output
        );
        assert!(
            !r.output.contains("gamma"),
            "common line gamma must be omitted: {}",
            r.output
        );
        assert!(
            !r.output.contains("delta"),
            "common line delta must be omitted: {}",
            r.output
        );
    }

    #[test]
    fn pure_addition_counts_adds_only() {
        let old = "one\ntwo\n";
        let new = "one\ntwo\nthree\nfour\n";
        let r = delta(old, new);
        assert_eq!(r.added, 2);
        assert_eq!(r.removed, 0);
        assert!(r.output.starts_with("+2/-0"));
    }

    #[test]
    fn pure_deletion_counts_removes_only() {
        let old = "one\ntwo\nthree\nfour\n";
        let new = "one\ntwo\n";
        let r = delta(old, new);
        assert_eq!(r.added, 0);
        assert_eq!(r.removed, 2);
        assert!(r.output.starts_with("+0/-2"));
    }

    #[test]
    fn oversize_line_count_bails_with_full_content() {
        let old = "seed\n";
        let mut new = String::new();
        for n in 0..(MAX_DELTA_LINES + 1) {
            new.push_str(&format!("line {n}\n"));
        }
        let r = delta(old, &new);

        assert!(r.bailed, "oversize input must bail");
        assert!(r.changed, "bail still reports a change");
        assert_eq!(r.added, 0, "no diff ops are computed on bail");
        assert_eq!(r.removed, 0);
        assert!(
            r.output.starts_with(BAIL_MARKER),
            "bail output must lead with the marker: {}",
            &r.output[..r.output.len().min(80)]
        );
        assert!(r.output.contains("line 0"));
        assert!(r.output.contains(&format!("line {}", MAX_DELTA_LINES)));
        assert_eq!(r.output, format!("{BAIL_MARKER}\n{new}"));
    }

    #[test]
    fn oversize_byte_count_bails() {
        let old = "small\n";
        let new = format!("{}\n", "x".repeat(MAX_DELTA_BYTES + 1));
        let r = delta(old, &new);
        assert!(r.bailed, "oversize bytes must bail");
        assert!(r.new_lines <= MAX_DELTA_LINES, "byte bail, not line bail");
        assert_eq!(r.output, format!("{BAIL_MARKER}\n{new}"));
    }

    #[test]
    fn at_line_limit_does_not_bail() {
        let mut old = String::new();
        let mut new = String::new();
        for n in 0..MAX_DELTA_LINES {
            old.push_str(&format!("line {n}\n"));
            new.push_str(&format!("line {n}\n"));
        }
        let new = new.replacen("line 0\n", "line 0 edited\n", 1);
        let r = delta(&old, &new);
        assert!(!r.bailed, "exactly at the line limit must not bail");
        assert_eq!(r.added, 1);
        assert_eq!(r.removed, 1);
    }
}
