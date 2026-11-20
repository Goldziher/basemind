use super::main_scan::{interrupted, reclaim_after_scan, scan_streaming, sync_git_history_after_scan};
use super::*;

pub(super) fn cmd_rescan(root: &std::path::Path, args: &RescanArgs, output: ScanOutput) -> Result<()> {
    let started = std::time::Instant::now();
    let root = &guard_workspace_root(root)?;
    let full = args.paths.is_empty();
    // ~keep Validate before touching any lock or daemon, so a bad path is a clean exit 2.
    let abs = if full {
        Vec::new()
    } else {
        basemind::cli::rescan::resolve_paths(root, &args.paths)?
    };
    let verbosity = output.verbosity;
    let mut out = output.human();
    // ~keep A running daemon is the sole index writer: hand it the scan instead of opening a second,
    // ~keep unlocked-by-it writer. Never fall through to a local scan after a daemon failure.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if basemind::cli::rescan::daemon_is_up() {
        let report = basemind::cli::rescan::rescan_via_daemon(root, (!full).then_some(abs), full)?;
        return output.forwarded(&mut out, "rescan", root, &report);
    }
    let cancel = basemind::cli::interrupt::install();
    bootstrap_grammars(&mut out, verbosity)?;
    let config = load_or_default(root)?;
    if let Some(notice) = writer_collision_notice(root) {
        return Err(basemind::cli::exit::CliExit::busy(notice));
    }
    let standalone_absent = !basemind::store::workspace_cache_dir(root)
        .join(basemind::store::VIEWS_DIR)
        .join(basemind::store::VIEW_WORKING)
        .join(basemind::store::INDEX_FILE)
        .is_file();
    let mut store = open_store_for_write(root, basemind::store::VIEW_WORKING, "rescan", LockHolder::Rescan)?;
    let full = full || standalone_absent || store.needs_full_scan();

    let report = if full {
        scan_streaming(
            output,
            basemind::scanner::ScanSource::WorkingTree,
            root,
            &mut store,
            &config,
            &cancel,
        )
        .context("rescan (full)")?
    } else {
        output
            .observed(|observer| {
                basemind::scanner::scan_named_paths_with_observer(
                    root,
                    &mut store,
                    &config,
                    &abs,
                    basemind::scanner::EmbedMode::Inline,
                    &cancel,
                    observer,
                    basemind::retrieval_wiring::backend(),
                )
            })
            .context("rescan (paths)")?
    };
    if report.cancelled {
        return interrupted(&mut out, &report, verbosity, "rescan");
    }
    output.summary(&mut out, root, &report, started)?;
    render::render_deps_unavailable(&mut out, &report.deps_unavailable, verbosity);
    reclaim_after_scan(&mut out, &report.stats, std::mem::take(&mut store.reset_stale_schema));
    sync_git_history_after_scan(root, !args.no_git_history, args.rebuild_git_history, &mut out);
    Ok(())
}

/// Print the counts a daemon reported for a forwarded `scan` or `rescan` (`what`).
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(super) fn print_forwarded(out: &mut impl std::io::Write, what: &str, report: &basemind::cli::rescan::RescanReport) {
    let _ = writeln!(
        out,
        "{what} via daemon: {} scanned, {} updated, {} docs indexed, {} removed ({} ms)",
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
}
