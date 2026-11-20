//! `basemind scan`: index the working tree, the git staging area or one revision, and report it as
//! lines on stdout or, with `--json`, as the response `index_rescan` gives. `basemind rescan`
//! (`main_rescan`) reports the same way through [`ScanOutput`].

use super::*;

/// How `scan` and `rescan` report: a line per file and a summary on stdout, or with `--json`
/// nothing on stdout but one JSON object once the scan ends.
#[derive(Clone, Copy, Debug)]
pub(super) struct ScanOutput {
    pub(super) verbosity: Verbosity,
    pub(super) no_color: bool,
    pub(super) json: bool,
}

impl ScanOutput {
    /// Where the human lines go: stdout, or nowhere under `--json`.
    pub(super) fn human(self) -> Box<dyn std::io::Write> {
        if self.json {
            Box::new(std::io::sink())
        } else {
            Box::new(render::stdout(self.no_color))
        }
    }

    /// Run `scan` with an observer that prints each file's line, or under `--json` one that prints
    /// nothing.
    pub(super) fn observed<R>(self, scan: impl FnOnce(&mut dyn basemind::scanner::ScanObserver) -> R) -> R {
        if self.json {
            return scan(&mut basemind::scanner::NullObserver);
        }
        let mut observer = render::ScanLineObserver::new(self.no_color, self.verbosity);
        let report = scan(&mut observer);
        observer.finish();
        report
    }

    /// Report a finished scan of `root` begun at `started`: the summary lines on `out`, or the JSON
    /// answer on stdout.
    pub(super) fn summary(
        self,
        out: &mut impl std::io::Write,
        root: &std::path::Path,
        report: &basemind::scanner::ScanReport,
        started: std::time::Instant,
    ) -> Result<()> {
        if self.json {
            let value = basemind::cli::rescan::scan_json(&report.stats, started.elapsed(), root)?;
            return basemind::cli::render::render_json(&value, &mut std::io::stdout().lock());
        }
        render::render_summary(out, &report.stats, self.verbosity);
        render::render_phase_peaks(out, &report.phase_peaks, self.verbosity);
        Ok(())
    }

    /// Report the counts the daemon returned for a forwarded `scan` or `rescan` (`what`) of `root`:
    /// the summary line on `out`, or the JSON answer on stdout.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(super) fn forwarded(
        self,
        out: &mut impl std::io::Write,
        what: &str,
        root: &std::path::Path,
        report: &basemind::cli::rescan::RescanReport,
    ) -> Result<()> {
        if self.json {
            let value = basemind::cli::rescan::forwarded_json(report, root)?;
            return basemind::cli::render::render_json(&value, &mut std::io::stdout().lock());
        }
        super::main_rescan::print_forwarded(out, what, report);
        Ok(())
    }
}

/// Build / refresh the repo-global git-history index after a working-tree scan (a separate phase
/// from the core file scan). Best-effort: a non-git dir, a disabled toggle, or any failure leaves
/// the index untouched and the history tools fall back to the live walk — never fails the scan.
pub(super) fn sync_git_history_after_scan(
    root: &std::path::Path,
    cli_enabled: bool,
    force_rebuild: bool,
    out: &mut impl std::io::Write,
) {
    if !cli_enabled || !basemind::git_history::index_enabled() {
        return;
    }
    let Ok(repo) = basemind::git::Repo::discover(root) else {
        return;
    };
    let basemind_dir = basemind::git_history::shared_history_basemind_dir(root);
    let index = match basemind::git_history::GitHistoryIndex::open(&basemind_dir) {
        Ok(index) => index,
        Err(error) => {
            tracing::warn!(?error, "git-history index unavailable; skipping");
            return;
        }
    };
    if force_rebuild && let Err(error) = index.clear() {
        tracing::warn!(?error, "git-history index clear failed");
    }
    match basemind::git_history::builder::sync(&index, &repo, &basemind_dir) {
        Ok(outcome) => {
            let summary = match outcome {
                basemind::git_history::builder::RebuildOutcome::Fresh => "git-history index: up to date".to_string(),
                basemind::git_history::builder::RebuildOutcome::Skipped { reason } => {
                    format!("git-history index: not updated ({reason})")
                }
                basemind::git_history::builder::RebuildOutcome::Incremental { added } => {
                    format!("git-history index: +{added} commits")
                }
                basemind::git_history::builder::RebuildOutcome::FullRebuild { reason, commits } => {
                    format!("git-history index: rebuilt {commits} commits ({reason})")
                }
            };
            let _ = writeln!(out, "{summary}");
        }
        Err(error) => tracing::warn!(?error, "git-history index sync failed"),
    }
}

/// Run a full scan, reporting each file as `output` says as the scan absorbs it.
///
/// The observer owns its own stdout stream, so it never collides with the caller's `out` and the
/// CLI never holds a per-file result for the whole repo just to print one line each. The caller
/// renders the summary from `report.stats` afterwards.
pub(super) fn scan_streaming(
    output: ScanOutput,
    source: basemind::scanner::ScanSource<'_>,
    root: &std::path::Path,
    store: &mut Store,
    config: &basemind::config::Config,
    cancel: &basemind::scanner::ScanCancel,
) -> std::result::Result<basemind::scanner::ScanReport, basemind::scanner::ScanError> {
    // The pass releases the embedding engine it loaded when the scan ends, so the model is not held
    // through the git-history sync and until exit.
    #[cfg(feature = "intelligence")]
    let _embed_pass = basemind::embeddings::begin_embed_pass();
    output.observed(|observer| {
        basemind::scanner::scan_with_observer(
            root,
            store,
            config,
            source,
            basemind::scanner::EmbedMode::Inline,
            cancel,
            observer,
            basemind::retrieval_wiring::backend(),
        )
    })
}

/// Turn an interrupted scan into the exit-`130` error after printing what did land. The scan stops
/// at a file boundary with its completed batches committed and the stale purge skipped, so the
/// index is consistent but incomplete — the message says to rerun.
pub(super) fn interrupted(
    out: &mut impl std::io::Write,
    report: &basemind::scanner::ScanReport,
    verbosity: Verbosity,
    what: &str,
) -> Result<()> {
    render::render_summary(out, &report.stats, verbosity);
    Err(basemind::cli::exit::CliExit::interrupted(format!(
        "{what} interrupted: files already indexed are committed and the index is consistent but incomplete; \
         rerun to finish"
    )))
}

pub(super) fn cmd_scan(
    root: &std::path::Path,
    args: &ScanArgs,
    matches: &clap::ArgMatches,
    output: ScanOutput,
) -> Result<()> {
    // ~keep Installed first: from here on Ctrl-C asks the scan to stop cleanly instead of killing the process.
    let cancel = basemind::cli::interrupt::install();
    let started = std::time::Instant::now();
    let root = &guard_workspace_root(root)?;
    let verbosity = output.verbosity;
    let mut out = output.human();
    #[cfg(all(feature = "comms", any(unix, windows)))]
    {
        let view_flag = if args.staged {
            Some("--staged")
        } else {
            args.rev.as_ref().map(|_| "--rev")
        };
        match view_flag {
            Some(flag) => main_scan_forward::refuse_view_scan_under_daemon(root, flag)?,
            None if main_scan_forward::forward_scan(root, args, matches, &cancel, output)? => return Ok(()),
            None => {}
        }
    }
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    let _ = matches;
    bootstrap_grammars(&mut out, verbosity)?;
    let config = load_or_default_with(root, Some(args.documents.clone()))?;
    #[cfg(feature = "intelligence")]
    bound_scan_ort_threads(&config);

    if args.staged {
        let repo = basemind::git::Repo::discover(root).context("`--staged` requires being inside a git repository")?;
        let mut store = open_store_for_write(root, basemind::store::VIEW_STAGED, "staged", LockHolder::Scan)?;
        render::render_scan_header(&mut out, "staged index", verbosity);
        let report = scan_streaming(
            output,
            basemind::scanner::ScanSource::Staged(&repo),
            root,
            &mut store,
            &config,
            &cancel,
        )
        .context("scan staged")?;
        if report.cancelled {
            return interrupted(&mut out, &report, verbosity, "scan --staged");
        }
        return output.summary(&mut out, root, &report, started);
    }
    if let Some(rev_spec) = &args.rev {
        let repo = basemind::git::Repo::discover(root).context("`--rev` requires being inside a git repository")?;
        let sha = repo.resolve_rev(rev_spec).context("resolve rev")?;
        let short = &sha[..7.min(sha.len())];
        let view = basemind::store::view_name_for_rev(short);
        let mut store = open_store_for_write(root, &view, "rev", LockHolder::Scan)?;
        render::render_scan_header(&mut out, &format!("rev {short}"), verbosity);
        let report = scan_streaming(
            output,
            basemind::scanner::ScanSource::Rev {
                repo: &repo,
                sha: sha.clone(),
            },
            root,
            &mut store,
            &config,
            &cancel,
        )
        .context("scan rev")?;
        if report.cancelled {
            return interrupted(&mut out, &report, verbosity, "scan --rev");
        }
        return output.summary(&mut out, root, &report, started);
    }

    if let Some(notice) = writer_collision_notice(root) {
        return Err(basemind::cli::exit::CliExit::busy(notice));
    }
    let mut store = open_store_for_write(root, basemind::store::VIEW_WORKING, "scan", LockHolder::Scan)?;
    let report = scan_streaming(
        output,
        basemind::scanner::ScanSource::WorkingTree,
        root,
        &mut store,
        &config,
        &cancel,
    )
    .context("scan")?;
    if report.cancelled {
        return interrupted(&mut out, &report, verbosity, "scan");
    }
    output.summary(&mut out, root, &report, started)?;
    render::render_deps_unavailable(&mut out, &report.deps_unavailable, verbosity);
    // This process exits next: flushed, the rows open as tables instead of a journal the next
    // writer (usually the daemon) replays into memtables and holds in its heap until they fill.
    if let Some(db) = store.index_db.as_ref() {
        db.reclaim_journal();
    }
    reclaim_after_scan(&mut out, &report.stats, std::mem::take(&mut store.reset_stale_schema));
    sync_git_history_after_scan(root, !args.no_git_history, args.rebuild_git_history, &mut out);
    Ok(())
}

/// Give a one-shot scan's ONNX sessions the cores of its scan pool (`[resources] scan_threads`, else
/// rayon's default), unless `[resources] embed_threads` names a count; see
/// [`basemind::embeddings::ort_intra_threads`].
#[cfg(feature = "intelligence")]
fn bound_scan_ort_threads(config: &basemind::config::Config) {
    let resources = &config.resources;
    let cores = match resources.scan_threads {
        0 => rayon::current_num_threads(),
        n => n,
    };
    let threads = basemind::embeddings::ort_intra_threads(
        basemind::embeddings::EmbedProcess::OneShotScan,
        resources.effective_embed_threads(config.documents.embed_max_threads),
        cores,
    );
    tracing::debug!(threads, cores, "ONNX intra-op threads for this scan");
    basemind::embeddings::bound_ort_memory(threads);
}

/// Reap what a migrating, re-extracting or schema-resetting pass orphaned. Gated on the pass having displaced anything,
/// so an ordinary no-op or edit-only scan never pays for the cross-workspace reference count.
pub(super) fn reclaim_after_scan(
    out: &mut impl std::io::Write,
    stats: &basemind::scanner::ScanStats,
    schema_reset: bool,
) {
    if !stats.displaced_artifacts() && !schema_reset {
        return;
    }
    match basemind::store_gc::reclaim_displaced_blobs(&basemind::data_root::DataRoot::current()) {
        Ok(report) => render::render_cleanup(out, &report),
        Err(error) => tracing::warn!(%error, "post-scan blob cleanup failed; the daemon's next sweep will retry"),
    }
}
