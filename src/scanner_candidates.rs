//! Candidate enumeration for a working-tree scan: the primary repository walk, the
//! `scan.extra_roots` walk, and the budgets that bound **both** of them.
//!
//! Split out of `scanner.rs` (module size cap) once the accounting stopped being a single counter.
//! One [`Budget`] now carries the `[scan] max_candidates` ceiling, the walk-entry ceiling derived
//! from it, and the contributor tallies that turn a breach into an actionable message — and it is
//! threaded through the extra-root walk as well. Before that, the ceiling covered only the primary
//! walk, so `scan.extra_roots` was an unbounded hole straight through the bound it advertises.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ahash::AHashMap;

use crate::config::Config;
use crate::config::root_guard::{self, RootRefusal};
use crate::scanner::{ScanCancel, ScanError};
use crate::scanner_filter::{Filters, ignore_walk_builder};

/// Candidate-count threshold above which the walk emits a visibility warning. Pure observability —
/// not a bound — so a large-but-legal scan is visible in the logs before it becomes a problem.
const LARGE_SCAN_CANDIDATE_WARN: usize = 50_000;

/// How many filesystem entries the walk may visit per unit of `[scan] max_candidates` before it is
/// judged to be walking a tree it will never finish. Deliberately generous: a healthy repo yields
/// roughly one entry per candidate plus its directories, so 20× only trips on a walk that is
/// enumerating vastly more than it keeps.
const VISIT_BUDGET_MULTIPLIER: usize = 20;

/// Floor under the derived walk budget, so a deliberately tiny `max_candidates` (tests, a narrow
/// index) does not turn every ordinary repository into a "walked too far" failure.
const MIN_VISIT_BUDGET: usize = 100_000;

/// Once the candidate ceiling is breached the walk keeps going for at most this many *additional*
/// entries, purely to finish the contributor tally. See [`Budget::accept`] for why the message is
/// worthless without it and why the extra work is bounded by entries rather than by the cap.
const SURVEY_VISIT_GRACE: usize = 200_000;

/// Ceiling on distinct labels held in one [`Tally`]. A runaway tree concentrates its files under a
/// handful of prefixes, so the offender is essentially always already present by the time the map
/// fills; the ceiling only stops a pathological "one directory per file" layout from turning the
/// diagnostic tally into its own memory problem.
const MAX_TALLY_KEYS: usize = 4096;

/// How many contributors the error message names.
const TOP_CONTRIBUTORS: usize = 5;

/// Label used for files sitting directly in the walk root. Bucketing them together keeps the
/// contributor list about *trees* — without it a flat repository produced a list of five unrelated
/// single-file entries, and a repository whose only root file is `basemind.toml` was told to
/// gitignore its own config.
const ROOT_FILES_LABEL: &str = "(files in the root)";

/// Operator opt-in for `scan.extra_roots`, mirroring the `BASEMIND_ALLOW_ANY_ROOT` /
/// `BASEMIND_ALLOW_PRIVATE_HOSTS` style. Truthy (`1`, `true`, `yes`) enables the feature; unset or
/// anything else makes every configured extra root a no-op, logged once per scan.
///
/// **Why the feature needs a grant at all.** `scan.extra_roots` names directories *outside* the
/// repository, and it is read from `<repo>/basemind.toml` — a file inside the tree being scanned,
/// therefore authored by whoever wrote the repository, not by the operator running basemind. Merely
/// cloning and indexing a repository was enough to make the scanner walk `~/.ssh` and `~/.aws`,
/// follow symlinks out of them, and surface their contents through the agent-facing `code grep` and
/// `search` tools. `basemind.toml` cannot be both the root guard's proof that "the operator meant
/// to index this" and an attacker-authored file present in every clone; the grant is what separates
/// the two roles.
///
/// **Why not "honour it only when it did not come from the repo's own config".** There is no other
/// config to come from. `config::merge_layers` takes its entire `File` layer from
/// `<root>/basemind.toml` (or the legacy `<root>/.basemind/basemind.toml`); there is no user-level
/// or machine-level config file, and the env/CLI override layer (`DocumentsCliOverrides`) covers
/// only `documents.*` and `llm.*`. Gating on provenance would mean "never honour `extra_roots`" in
/// every real deployment — removing the feature rather than containing it.
///
/// **Why not "require each extra root to pass the full root guard".** An attacker cannot plant a
/// `basemind.toml` in `~/.ssh`, so that test does block the crown jewels — but it admits every
/// other git checkout on the machine (a private work repo, `~/.dotfiles`, `$HOME` itself when it is
/// a repo) and CI paths are guessable. It narrows the hole instead of closing it, and it would
/// break the feature's main legitimate user anyway: a Bazel `external/` cache is neither a git
/// repository nor a basemind project.
///
/// **The trade-off.** A legitimate `extra_roots` user must now set this variable once in the
/// environment that launches basemind (shell, MCP host config, CI job). A cloned repository gains
/// nothing by asking for extra roots, because the process environment is the one input it cannot
/// write. The per-root [`RootRefusal::FilesystemRoot`] refusal below still applies even with the
/// grant: the grant says "this operator uses extra roots", never "walk an entire volume".
pub const ALLOW_EXTRA_ROOTS_ENV: &str = "BASEMIND_ALLOW_EXTRA_ROOTS";

/// Process-wide programmatic form of [`ALLOW_EXTRA_ROOTS_ENV`], for embedders that configure
/// basemind in code rather than through the environment (and for the test suite, which must not
/// mutate the process environment out from under parallel threads).
static EXTRA_ROOTS_GRANT: AtomicBool = AtomicBool::new(false);

/// Grant or revoke `scan.extra_roots` for this process. Equivalent to [`ALLOW_EXTRA_ROOTS_ENV`],
/// and equally out of reach of the scanned repository: it is an API call by the embedding program,
/// never a config value. Idempotent.
pub fn allow_extra_roots(allow: bool) {
    EXTRA_ROOTS_GRANT.store(allow, Ordering::Relaxed);
}

fn extra_roots_granted() -> bool {
    EXTRA_ROOTS_GRANT.load(Ordering::Relaxed) || std::env::var(ALLOW_EXTRA_ROOTS_ENV).is_ok_and(|v| is_truthy(&v))
}

/// `1` / `true` / `yes`, case- and whitespace-insensitive. Matches `root_guard`'s private
/// equivalent; duplicated rather than shared because that module is a stable, narrowly-scoped
/// public surface and its parser is not part of it.
fn is_truthy(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("1") || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

/// Counts per contributor label, bounded by [`MAX_TALLY_KEYS`].
#[derive(Default)]
struct Tally(AHashMap<String, usize>);

impl Tally {
    /// Allocation-free for a label already present — the common case, and this runs once per walk
    /// entry.
    fn bump(&mut self, label: &str) {
        if let Some(count) = self.0.get_mut(label) {
            *count += 1;
        } else if self.0.len() < MAX_TALLY_KEYS {
            self.0.insert(label.to_string(), 1);
        }
    }

    /// The heaviest labels, descending by count (name breaks ties so the message is deterministic).
    fn top(self) -> Vec<(String, usize)> {
        let mut top: Vec<(String, usize)> = self.0.into_iter().collect();
        top.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top.truncate(TOP_CONTRIBUTORS);
        top
    }
}

/// Whether the caller should keep walking.
#[derive(PartialEq, Eq, Debug)]
enum Flow {
    Continue,
    Stop,
}

/// The candidate set under construction, plus everything needed to explain an abort.
///
/// Two independent bounds, because they have completely different fixes:
///
/// * **candidates accepted** — files that passed the include/exclude gate and would be indexed.
///   Exceeding `[scan] max_candidates` means the index itself is too big: exclude a tree.
/// * **entries visited** — every entry the walker yielded, candidate or not, directory or file.
///   Exceeding the derived walk budget means the *walk* is too big even though it is keeping almost
///   nothing: the root is too broad, or the excludes are file-shaped (`**/generated`) so whole
///   trees are stat'd one file at a time instead of being pruned at the directory.
///
/// Both bounds are disabled when `max_candidates` is `0`, and both span the primary walk and the
/// `scan.extra_roots` walk.
struct Budget {
    cap: usize,
    /// Derived from `cap`; `0` when the ceiling is disabled.
    visit_budget: usize,
    keys: Vec<String>,
    candidates: Tally,
    visits: Tally,
    accepted: usize,
    visited: usize,
    over_cap: bool,
    over_walk: bool,
    /// `visited` value at which the post-breach contributor survey stops. Only meaningful once
    /// `over_cap` is set.
    survey_until: usize,
}

impl Budget {
    fn new(cap: usize) -> Self {
        let visit_budget = if cap == 0 {
            0
        } else {
            cap.saturating_mul(VISIT_BUDGET_MULTIPLIER).max(MIN_VISIT_BUDGET)
        };
        Self {
            cap,
            visit_budget,
            keys: Vec::new(),
            candidates: Tally::default(),
            visits: Tally::default(),
            accepted: 0,
            visited: 0,
            over_cap: false,
            over_walk: false,
            survey_until: 0,
        }
    }

    /// Account for one yielded walk entry of any kind. Call before deciding whether it is a
    /// candidate.
    fn visit(&mut self, label: &str) -> Flow {
        self.visited += 1;
        self.visits.bump(label);
        if self.over_cap {
            // The candidate ceiling already decided this scan's fate; the walk budget would only
            // replace a precise diagnosis with a vaguer one, so the survey window governs instead. ~keep
            return if self.visited > self.survey_until {
                Flow::Stop
            } else {
                Flow::Continue
            };
        }
        if self.visit_budget != 0 && self.visited > self.visit_budget {
            self.over_walk = true;
            return Flow::Stop;
        }
        Flow::Continue
    }

    /// Account for one entry that passed the filters and would be indexed.
    ///
    /// The ceiling is evaluated *after* the count, so a repository holding exactly
    /// `max_candidates` files completes: the old check ran at the top of the walk loop against
    /// every entry, so once the candidate `Vec` reached the cap the next directory node — or
    /// gitignored file, or non-included file — aborted a scan that never exceeded anything.
    ///
    /// Past the ceiling the walk continues, without storing keys, for a bounded window. The tally
    /// is the only thing the operator gets to act on, and `ignore::Walk` is depth-first in readdir
    /// order, so stopping at the breach names whichever directory happened to be enumerated
    /// first — on a repo with `aaa/` (30 files) and `zzz/` (500k files) it named `aaa`. The window
    /// is counted in *entries visited*, not in candidates or as a multiple of the cap, so the extra
    /// work is the same small constant whether the cap is 20 or 500 000.
    fn accept(&mut self, label: &str, key: &str) -> Flow {
        self.accepted += 1;
        self.candidates.bump(label);
        if self.cap == 0 || self.accepted <= self.cap {
            self.keys.push(key.to_string());
            return Flow::Continue;
        }
        if !self.over_cap {
            self.over_cap = true;
            self.survey_until = self.visited.saturating_add(SURVEY_VISIT_GRACE);
        }
        if self.visited > self.survey_until {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    /// The candidate keys collected so far, breaches ignored. Used on cancellation, where the
    /// caller discards the pass anyway.
    fn into_keys(self) -> Vec<String> {
        self.keys
    }

    /// The candidate set, or the breach that ended the walk. `TooManyCandidates` wins over
    /// `WalkTooLarge`: it is the more specific diagnosis, and the walk budget stops applying the
    /// moment the candidate ceiling is breached.
    fn finish(self, root: &Path) -> Result<Vec<String>, ScanError> {
        if self.over_cap {
            return Err(ScanError::TooManyCandidates {
                candidates: self.accepted,
                cap: self.cap,
                root: root.display().to_string(),
                top_dirs: self.candidates.top(),
            });
        }
        if self.over_walk {
            return Err(ScanError::WalkTooLarge {
                visited: self.visited,
                candidates: self.accepted,
                cap: self.cap,
                root: root.display().to_string(),
                top_dirs: self.visits.top(),
            });
        }
        Ok(self.keys)
    }
}

/// Contributor label for a walk-root-relative path: the first **two** segments (`packages/app`),
/// the single segment when there is only one directory level (`src`), or [`ROOT_FILES_LABEL`] for a
/// file directly in the root.
///
/// Two segments rather than one because on a monorepo everything lives under `packages/` or
/// `services/`, and `packages (500000)` tells the operator nothing they did not already know.
/// Borrowed from `rel`, so labelling costs nothing per entry.
fn label_for(rel: &str) -> &str {
    let Some(first) = rel.find('/') else {
        return ROOT_FILES_LABEL;
    };
    match rel[first + 1..].find('/') {
        Some(second) => &rel[..first + 1 + second],
        None => &rel[..first],
    }
}

/// `path` relative to `base` in forward-slash form, or `None` when it is not under `base` or not
/// valid UTF-8. `buf` backs the Windows separator rewrite; on Unix the result borrows `path`.
fn rel_str<'a>(base: &Path, path: &'a Path, buf: &'a mut String) -> Option<&'a str> {
    let rel = path.strip_prefix(base).ok()?.to_str()?;
    #[cfg(windows)]
    {
        buf.clear();
        buf.push_str(&rel.replace('\\', "/"));
        Some(buf.as_str())
    }
    #[cfg(not(windows))]
    {
        let _ = buf;
        Some(rel)
    }
}

/// Enumerate the working tree's candidate files.
///
/// Directories excluded by the glob floor / `[scan] exclude` / a skipped submodule root are pruned
/// by [`Filters::dir_pruner`] *at the directory*, so a non-gitignored `node_modules` is never
/// descended into — see [`crate::scanner_filter`] for why that keeps the candidate set identical.
/// The walk stops on a tripped [`ScanCancel`] (a draining daemon must be able to interrupt a
/// runaway walk) and on either arm of [`Budget`], which fails the scan rather than letting the
/// candidate `Vec` — or the walk that fills it — run away.
pub(crate) fn walk_candidates(
    root: &Path,
    config: &Config,
    filters: &Filters,
    cancel: &ScanCancel,
) -> Result<Vec<String>, ScanError> {
    let mut budget = Budget::new(config.scan.max_candidates);
    let pruner = filters.dir_pruner(Some(root));
    let walker = ignore_walk_builder(root, config.scan.respect_gitignore, config.scan.follow_symlinks)
        .filter_entry(move |dent| pruner.keep(dent))
        .build();
    let mut buf = String::new();
    for dent in walker.flatten() {
        if cancel.is_cancelled() {
            // A tripped token means "stop walking", and that has to include the extra roots —
            // otherwise the check buys nothing whenever an extra root is the runaway tree. Returning
            // the partial list is safe: `scan_with_cancel` returns before the stale purge whenever
            // the token is tripped, so the candidates this walk never reached are not mistaken for
            // deletions. It also outranks a pending breach: a drained scan is discarded, and
            // reporting a ceiling error for a walk we deliberately cut short would be a lie. ~keep
            return Ok(budget.into_keys());
        }
        let path = dent.path();
        let Some(rel) = rel_str(root, path, &mut buf) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        let label = label_for(rel);
        if budget.visit(label) == Flow::Stop {
            break;
        }
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if !filters.allows(rel) {
            continue;
        }
        if budget.accept(label, rel) == Flow::Stop {
            break;
        }
    }
    if !budget.over_cap && !budget.over_walk && !cancel.is_cancelled() {
        walk_extra_roots(root, config, filters, cancel, &mut budget);
    }
    if cancel.is_cancelled() {
        return Ok(budget.into_keys());
    }
    let out = budget.finish(root)?;
    if out.len() > LARGE_SCAN_CANDIDATE_WARN {
        tracing::warn!(
            candidates = out.len(),
            "scan candidate set is very large; check .gitignore / [scan] exclude globs for generated or vendored trees"
        );
    }
    Ok(out)
}

/// Walk each configured `scan.extra_roots` directory and append its files to `budget`, keyed by
/// **absolute** path (see `RelPath::is_external`). Extra roots live outside the repo, so there is
/// no `strip_prefix(root)` — the absolute path *is* the index key, which never collides with the
/// repo's relative keys.
///
/// Every root is vetted by [`vet_extra_root`], the whole feature is gated on
/// [`ALLOW_EXTRA_ROOTS_ENV`], and the walk shares the caller's [`Budget`] and [`ScanCancel`] so an
/// extra root can neither escape `[scan] max_candidates` nor outlive a drain request. Contributors
/// are labelled by their absolute path, so a cap breach names the extra root that caused it rather
/// than blaming the repository.
fn walk_extra_roots(root: &Path, config: &Config, filters: &Filters, cancel: &ScanCancel, budget: &mut Budget) {
    if config.scan.extra_roots.is_empty() {
        return;
    }
    if !extra_roots_granted() {
        tracing::warn!(
            roots = config.scan.extra_roots.len(),
            env = ALLOW_EXTRA_ROOTS_ENV,
            "scan.extra_roots ignored: it names directories outside the repository but was read from \
             the repository's own basemind.toml; set {ALLOW_EXTRA_ROOTS_ENV}=1 in the environment to \
             opt in",
        );
        return;
    }
    let repo_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for raw_root in &config.scan.extra_roots {
        if cancel.is_cancelled() {
            return;
        }
        let Some(extra) = vet_extra_root(raw_root, &repo_root) else {
            continue;
        };
        let Some(root_display) = extra.to_str().map(display_key) else {
            tracing::warn!(root = %extra.display(), "extra_root skipped: path is not valid UTF-8");
            continue;
        };
        let pruner = filters.dir_pruner(None);
        // `scan.follow_symlinks` (default `false`) governs here too. Following links used to be
        // hard-coded on, which is strictly worse out of an extra root than inside the repo: the
        // repo walk at least starts from a tree the operator chose and is bounded by that tree's own
        // `.gitignore`, whereas an extra root is named by a file *inside the scanned repository*, so
        // a followed link turns one named directory into arbitrary reach across the filesystem —
        // through a target that appears in no config the operator ever reviewed. ~keep
        let walker = ignore_walk_builder(&extra, config.scan.respect_gitignore, config.scan.follow_symlinks)
            .filter_entry(move |dent| pruner.keep(dent))
            .build();
        let mut rel_buf = String::new();
        let mut key_buf = String::new();
        let mut label_buf = String::new();
        for dent in walker.flatten() {
            if cancel.is_cancelled() {
                return;
            }
            let path = dent.path();
            let Some(rel) = rel_str(&extra, path, &mut rel_buf) else {
                continue;
            };
            let label = extra_label(&mut label_buf, &root_display, rel);
            if budget.visit(label) == Flow::Stop {
                return;
            }
            if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let Some(key) = key_str(path, &mut key_buf) else {
                continue;
            };
            if !filters.allows(key) {
                continue;
            }
            if budget.accept(label, key) == Flow::Stop {
                return;
            }
        }
    }
}

/// Forward-slash form of a path string, matching the index-key convention.
fn display_key(raw: &str) -> String {
    #[cfg(windows)]
    {
        raw.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        raw.to_string()
    }
}

/// An extra-root file's index key: its absolute path in forward-slash form. Mirrors [`rel_str`] —
/// `buf` backs the Windows rewrite and is untouched elsewhere, so no allocation per entry on Unix.
fn key_str<'a>(path: &'a Path, buf: &'a mut String) -> Option<&'a str> {
    let raw = path.to_str()?;
    #[cfg(windows)]
    {
        buf.clear();
        buf.push_str(&raw.replace('\\', "/"));
        Some(buf.as_str())
    }
    #[cfg(not(windows))]
    {
        let _ = buf;
        Some(raw)
    }
}

/// Contributor label for an extra-root file: the root's own path, extended by the same first-two-
/// segments rule applied *within* the root. Absolute, so the message is unambiguous about which
/// `extra_roots` entry to narrow.
fn extra_label<'a>(buf: &'a mut String, root_display: &str, rel: &str) -> &'a str {
    buf.clear();
    buf.push_str(root_display);
    let sub = label_for(rel);
    if sub != ROOT_FILES_LABEL {
        buf.push('/');
        buf.push_str(sub);
    }
    buf.as_str()
}

/// Vet one configured extra root, returning the canonical directory to walk or `None` with a
/// warning. Refusals are skips, not scan failures, matching how this feature has always treated a
/// missing or inside-the-repo root: one bad entry must not take down an otherwise valid scan.
///
/// Canonicalization runs first so a symlinked root is judged by its target — `extra_roots = ["/tmp/
/// anywhere"]` pointing at `/` is refused as the filesystem root it resolves to. The refusal itself
/// is [`root_guard`]'s, not a local reimplementation, so extra roots inherit the one rule that
/// module declares unoverridable. [`RootRefusal::NotAProject`] is deliberately *not* enforced: the
/// [`ALLOW_EXTRA_ROOTS_ENV`] grant already establishes operator intent, and the feature's main
/// legitimate target — a Bazel `external/` cache — is neither a git repository nor a basemind
/// project, so requiring project-ness would refuse the use case without closing anything the grant
/// leaves open.
fn vet_extra_root(raw_root: &Path, repo_root: &Path) -> Option<PathBuf> {
    let extra = match raw_root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(root = %raw_root.display(), error = %e, "extra_root skipped: cannot access");
            return None;
        }
    };
    if let Err(RootRefusal::FilesystemRoot) = root_guard::workspace_root_verdict(&extra) {
        tracing::warn!(
            root = %extra.display(),
            "extra_root refused: that is a filesystem or volume root; indexing it would walk the entire machine"
        );
        return None;
    }
    if !extra.is_dir() {
        tracing::warn!(root = %extra.display(), "extra_root skipped: not a directory");
        return None;
    }
    if extra.starts_with(repo_root) {
        tracing::warn!(root = %extra.display(), "extra_root skipped: inside the repository root (already indexed)");
        return None;
    }
    Some(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_takes_two_segments_and_buckets_root_files() {
        assert_eq!(label_for("packages/app/src/x.rs"), "packages/app");
        assert_eq!(label_for("src/a.rs"), "src");
        assert_eq!(label_for("src"), ROOT_FILES_LABEL);
        assert_eq!(label_for("basemind.toml"), ROOT_FILES_LABEL);
    }

    /// A repository holding *exactly* `max_candidates` files must scan. The old check ran against
    /// every walk entry, so once the candidate list reached the cap the next entry of any kind —
    /// a directory node, a gitignored file — aborted a scan that never exceeded the ceiling. Which
    /// entry came next was readdir order, making it an intermittent spurious failure.
    #[test]
    fn exactly_cap_candidates_is_not_a_breach() {
        let mut budget = Budget::new(3);
        for i in 0..3 {
            assert_eq!(budget.visit("src"), Flow::Continue);
            assert_eq!(budget.accept("src", &format!("src/f{i}.rs")), Flow::Continue);
        }
        for _ in 0..50 {
            assert_eq!(
                budget.visit("src"),
                Flow::Continue,
                "non-candidate entries never breach"
            );
        }
        assert_eq!(budget.finish(Path::new("/repo")).expect("no breach").len(), 3);
    }

    /// One candidate past the cap is a breach, the reported count is the number actually counted
    /// (not the cap parroted back), and the survey keeps tallying so the heaviest tree is named
    /// even though it is enumerated after the breach.
    #[test]
    fn breach_survives_to_name_the_real_offender() {
        let mut budget = Budget::new(20);
        for i in 0..30 {
            budget.visit("aaa");
            budget.accept("aaa", &format!("aaa/f{i}.rs"));
        }
        for i in 0..5000 {
            budget.visit("zzz");
            budget.accept("zzz", &format!("zzz/f{i}.rs"));
        }
        let err = budget.finish(Path::new("/repo")).expect_err("cap breached");
        match err {
            ScanError::TooManyCandidates {
                candidates,
                cap,
                top_dirs,
                ..
            } => {
                assert_eq!(cap, 20);
                assert_eq!(candidates, 5030, "the honest count, not the cap");
                assert_eq!(top_dirs.first().map(|(n, c)| (n.as_str(), *c)), Some(("zzz", 5000)));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    /// The survey window is bounded in entries, so a cap breach cannot turn into an unbounded walk.
    #[test]
    fn survey_window_stops_the_walk() {
        let mut budget = Budget::new(1);
        budget.visit("a");
        assert_eq!(budget.accept("a", "a/one.rs"), Flow::Continue);
        budget.visit("a");
        assert_eq!(budget.accept("a", "a/two.rs"), Flow::Continue, "survey opens");
        for _ in 0..SURVEY_VISIT_GRACE {
            budget.visit("a");
        }
        assert_eq!(budget.visit("a"), Flow::Stop, "survey window closed");
        assert_eq!(budget.keys.len(), 1, "nothing past the cap is stored");
    }

    /// A walk that visits far more than it keeps is bounded on its own terms, and reports the
    /// entries it burned rather than the handful of candidates it found.
    #[test]
    fn walk_budget_bounds_a_walk_that_accepts_almost_nothing() {
        let mut budget = Budget::new(1_000_000);
        let limit = budget.visit_budget;
        assert_eq!(limit, 20_000_000);
        budget.visited = limit - 1;
        assert_eq!(budget.visit("noise/tree"), Flow::Continue);
        assert_eq!(budget.visit("noise/tree"), Flow::Stop);
        match budget.finish(Path::new("/repo")).expect_err("walk budget breached") {
            ScanError::WalkTooLarge {
                visited,
                candidates,
                top_dirs,
                ..
            } => {
                assert_eq!(visited, limit + 1);
                assert_eq!(candidates, 0);
                assert_eq!(top_dirs.first().map(|(n, _)| n.as_str()), Some("noise/tree"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    /// `max_candidates = 0` disables both arms, not just the candidate one.
    #[test]
    fn zero_cap_disables_both_bounds() {
        let mut budget = Budget::new(0);
        assert_eq!(budget.visit_budget, 0);
        for i in 0..1000 {
            assert_eq!(budget.visit("a"), Flow::Continue);
            assert_eq!(budget.accept("a", &format!("a/{i}.rs")), Flow::Continue);
        }
        assert_eq!(budget.finish(Path::new("/repo")).expect("unlimited").len(), 1000);
    }

    #[test]
    fn tally_stops_growing_but_keeps_counting_known_labels() {
        let mut tally = Tally::default();
        for i in 0..(MAX_TALLY_KEYS + 100) {
            tally.bump(&format!("d{i}"));
        }
        tally.bump("d0");
        let top = tally.top();
        assert_eq!(top.first().map(|(n, c)| (n.as_str(), *c)), Some(("d0", 2)));
    }

    /// The filesystem root is refused even through a symlink, because vetting canonicalizes first.
    #[test]
    #[cfg(unix)]
    fn filesystem_root_is_refused_even_via_a_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let link = tmp.path().join("to-root");
        std::os::unix::fs::symlink("/", &link).expect("symlink");
        let repo = tmp.path().canonicalize().expect("canonicalize");
        assert!(vet_extra_root(Path::new("/"), &repo).is_none());
        assert!(
            vet_extra_root(&link, &repo).is_none(),
            "a symlink to / resolves to / and is refused"
        );
    }

    #[test]
    fn truthy_grant_values() {
        for yes in ["1", "true", " TRUE ", "yes"] {
            assert!(is_truthy(yes), "{yes:?}");
        }
        for no in ["0", "false", "no", ""] {
            assert!(!is_truthy(no), "{no:?}");
        }
    }
}
