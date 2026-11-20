//! The rule every check of what an added worktree costs applies, shared by
//! `tests/worktree_lifecycle_cli.rs` and the multi-worktree mode of `tests/harden.rs` so the two
//! cannot disagree on where the bound sits. Each check sets its own percent from its own data.

/// Whether an added worktree's `added` bytes exceed `percent` percent of the first worktree's
/// `first`. Exactly `percent` percent is within the bound.
pub fn exceeds_growth_bound(added: u64, first: u64, percent: u64) -> bool {
    u128::from(added) * 100 > u128::from(first) * u128::from(percent)
}
