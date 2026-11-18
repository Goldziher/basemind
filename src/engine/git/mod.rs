//! The git operations behind `git_log`, `git_blame`, `git_diff` and the CLI's `git churn` /
//! `git by-path`: synchronous reads of the workspace's repository, its git cache and its
//! git-history index.
//!
//! `log.rs` holds the three tool bodies and the commit-log family (`status`, `recent`, `touching`,
//! `churn`, `search`); `file.rs` the operations scoped to one path (`diff`, `diff_outline`, `blame`,
//! `blame_symbol`, `symbol_history`); `by_path.rs` the windowed pickaxe; `show.rs` one commit in
//! full and the commit-list budget; `fingerprint.rs` the symbol-body hashes symbol history compares.

mod by_path;
mod common;
mod file;
mod fingerprint;
mod log;
mod show;
