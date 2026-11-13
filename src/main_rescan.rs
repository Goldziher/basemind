use super::*;

pub(super) fn cmd_rescan(
    root: &std::path::Path,
    args: &RescanArgs,
    verbosity: Verbosity,
    no_color: bool,
) -> Result<()> {
    let root = &guard_workspace_root(root)?;
    let full = args.full || args.paths.is_empty();
    // ~keep Validate before touching any lock or daemon, so a bad path is a clean exit 2.
    let abs = if full {
        Vec::new()
    } else {
        basemind::cli::rescan::resolve_paths(root, &args.paths)?
    };
    let mut out = render::stdout(no_color);
    // ~keep A running daemon is the sole index writer: hand it the scan instead of opening a second,
    // ~keep unlocked-by-it writer. Never fall through to a local scan after a daemon failure.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if basemind::cli::rescan::daemon_is_up() {
        use std::io::Write as _;
        let report = basemind::cli::rescan::rescan_via_daemon(root, (!full).then_some(abs), full)?;
        let _ = writeln!(
            out,
            "rescan via daemon: {} scanned, {} updated, {} docs indexed, {} removed ({} ms)",
            report.scanned, report.updated, report.docs_indexed, report.removed, report.elapsed_ms
        );
        if report.docs_degraded > 0 || report.doc_timeouts > 0 {
            let _ = writeln!(
                out,
                "docs_degraded {}  doc_timeouts {} (see [documents] markdown_chunk_max_bytes)",
                report.docs_degraded, report.doc_timeouts
            );
        }
        if report.docs_text_fallback > 0 {
            let _ = writeln!(
                out,
                "docs_text_fallback {} (of docs_degraded: read as plain text after a structured-extraction failure)",
                report.docs_text_fallback
            );
        }
        return Ok(());
    }
    let cancel = basemind::cli::interrupt::install();
    bootstrap_grammars(verbosity, no_color)?;
    let config = load_or_default(root)?;
    if let Some(notice) = writer_collision_notice(root) {
        return Err(basemind::cli::exit::CliExit::busy(notice));
    }
    let shared_index = std::env::var_os("BASEMIND_INDEX_LAYERED").is_some_and(|value| value == "1");
    if shared_index && !full && !cancel.is_cancelled() {
        match basemind::store_base::refresh_local_worktree(root, &config, LockHolder::Rescan) {
            Ok(true) if !cancel.is_cancelled() => {
                eprintln!("Shared worktree index ready.");
                sync_git_history_after_scan(root, !args.no_git_history, args.rebuild_git_history, &mut out);
                return Ok(());
            }
            Err(error)
                if error
                    .downcast_ref::<basemind::store::StoreError>()
                    .is_some_and(basemind::store::StoreError::is_lock_contention) =>
            {
                return Err(basemind::cli::exit::CliExit::busy(
                    basemind::store::LOCK_CONTENTION_HELP,
                ));
            }
            _ => {}
        }
    }
    let standalone_absent = !basemind::store::workspace_cache_dir(root)
        .join(basemind::store::VIEWS_DIR)
        .join(basemind::store::VIEW_WORKING)
        .join(basemind::store::INDEX_FILE)
        .is_file();
    let mut store = open_store_for_write(root, basemind::store::VIEW_WORKING, "rescan", LockHolder::Rescan)?;
    let full = full || standalone_absent || store.index_rebuild_needed;

    let report = if full {
        scan_streaming(
            no_color,
            verbosity,
            basemind::scanner::ScanSource::WorkingTree,
            root,
            &mut store,
            &config,
            &cancel,
        )
        .context("rescan (full)")?
    } else {
        let mut observer = render::ScanLineObserver::new(no_color, verbosity);
        let report = basemind::scanner::scan_paths_with_observer(
            root,
            &mut store,
            &config,
            &abs,
            basemind::scanner::EmbedMode::Inline,
            &cancel,
            &mut observer,
        );
        observer.finish();
        report.context("rescan (paths)")?
    };
    if report.cancelled {
        return interrupted(&mut out, &report, verbosity, "rescan");
    }
    render::render_summary(&mut out, &report.stats, verbosity);
    render::render_phase_peaks(&mut out, &report.phase_peaks, verbosity);
    reclaim_after_scan(&mut out, &report.stats, std::mem::take(&mut store.reset_stale_schema));
    sync_git_history_after_scan(root, !args.no_git_history, args.rebuild_git_history, &mut out);
    Ok(())
}
