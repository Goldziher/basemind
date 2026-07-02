//! Colored, colorblind-safe rendering for scan/watch reports.
//!
//! Every line carries a symbol AND a label so color is decoration, not the
//! information channel — users with color-vision differences read the same
//! signal as everyone else.
//!
//! Auto-disables ANSI on non-TTY stdout and when `NO_COLOR` is set.

use std::io::Write;

use anstream::AutoStream;
use anstyle::{AnsiColor, Color, Reset, Style};

use crate::lang::BootstrapSummary;
use crate::scanner::{FileResult, FileStatus, ScanReport, ScanStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Default,
    Verbose,
}

impl Verbosity {
    pub fn from_flags(quiet: bool, verbose: bool) -> Self {
        match (quiet, verbose) {
            (true, _) => Verbosity::Quiet,
            (_, true) => Verbosity::Verbose,
            _ => Verbosity::Default,
        }
    }
}

/// Stdout writer that auto-handles TTY detection + NO_COLOR.
/// `force_off=true` strips ANSI regardless of TTY state (for `--no-color`).
pub fn stdout(force_off: bool) -> AutoStream<std::io::Stdout> {
    if force_off {
        AutoStream::never(std::io::stdout())
    } else {
        AutoStream::auto(std::io::stdout())
    }
}

const COL_PATH: usize = 8; // "✓ ok  " column width before the path

/// Print every line of a report, respecting verbosity.
pub fn render_report(w: &mut AutoStream<std::io::Stdout>, report: &ScanReport, verbosity: Verbosity) {
    for r in &report.results {
        render_file(w, r, verbosity);
    }
    render_summary(w, &report.stats, verbosity);
}

/// Print only the per-file lines (no summary). Used by the watcher per batch.
pub fn render_lines(w: &mut AutoStream<std::io::Stdout>, report: &ScanReport, verbosity: Verbosity) {
    for r in &report.results {
        render_file(w, r, verbosity);
    }
}

pub fn render_file(w: &mut AutoStream<std::io::Stdout>, res: &FileResult, verbosity: Verbosity) {
    let Some(line) = format_line(res, verbosity) else {
        return;
    };
    let _ = writeln!(w, "{line}");
}

pub fn render_summary(w: &mut AutoStream<std::io::Stdout>, stats: &ScanStats, verbosity: Verbosity) {
    if verbosity == Verbosity::Quiet
        && stats.read_failed == 0
        && stats.extract_failed == 0
        && stats.updated_with_warnings == 0
    {
        // Nothing the user needs to see.
        return;
    }
    let style_label = Style::new().dimmed();
    let style_zero = Style::new().dimmed();
    let style_ok = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    let style_warn = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    let style_fail = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));

    let pair = |label: &str, n: usize, important: Style| -> String {
        let s = if n == 0 { style_zero } else { important };
        format!(
            "{label_style}{label}{label_off} {n_style}{n}{n_off}",
            label_style = style_label.render(),
            label_off = Reset.render(),
            label = label,
            n_style = s.render(),
            n_off = Reset.render(),
            n = n,
        )
    };

    let line = format!(
        "{scanned}  {updated}  {warn}  {unchanged}  {failed}  {skipped}  {removed}",
        scanned = pair("scanned", stats.scanned, Style::new()),
        updated = pair("updated", stats.updated, style_ok),
        warn = pair("warn", stats.updated_with_warnings, style_warn),
        unchanged = pair("unchanged", stats.skipped_unchanged, Style::new()),
        failed = pair("failed", stats.read_failed + stats.extract_failed, style_fail),
        skipped = pair(
            "skipped",
            stats.skipped_too_large + stats.skipped_non_utf8 + stats.skipped_no_lang,
            Style::new(),
        ),
        removed = pair("removed", stats.removed, style_warn),
    );
    let _ = writeln!(w, "{line}");
    if stats.docs_indexed > 0 {
        let docs_line = pair("docs_indexed", stats.docs_indexed, style_ok);
        let _ = writeln!(w, "{docs_line}");
    }
}

/// Print a one-line summary of the grammar bootstrap.
///
/// Silent when all grammars were already cached (unless verbose). When a download did happen,
/// always emit at least one line so the user knows what basemind was doing during the pause.
pub fn render_grammar_bootstrap(w: &mut AutoStream<std::io::Stdout>, summary: &BootstrapSummary, verbosity: Verbosity) {
    if !summary.did_download() && verbosity != Verbosity::Verbose {
        return;
    }
    let dim = Style::new().dimmed();
    let ok = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    let info = Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlue)));

    if summary.did_download() {
        let names = summary.downloaded.join(", ");
        let _ = writeln!(
            w,
            "{s}▼ downloaded {n} grammar{plural}{r} {d}({names}){dr}",
            s = info.render(),
            r = Reset.render(),
            d = dim.render(),
            dr = Reset.render(),
            n = summary.downloaded.len(),
            plural = if summary.downloaded.len() == 1 { "" } else { "s" },
            names = names,
        );
    }
    if verbosity == Verbosity::Verbose {
        let cached_count = summary.already_cached.len();
        let _ = writeln!(
            w,
            "{s}✓ grammars ready{r} {d}({cached_count} cached, {dl} fresh){dr}",
            s = ok.render(),
            r = Reset.render(),
            d = dim.render(),
            dr = Reset.render(),
            cached_count = cached_count,
            dl = summary.downloaded.len(),
        );
        if let Some(dir) = &summary.cache_dir {
            let _ = writeln!(
                w,
                "  {d}cache: {dir}{dr}",
                d = dim.render(),
                dr = Reset.render(),
                dir = dir.display(),
            );
        }
    }
}

/// Tiny header used by non-working-tree scans (staged / rev). Working-tree scans don't get
/// one — they're the default and shouldn't grow noise on every run.
pub fn render_scan_header(w: &mut AutoStream<std::io::Stdout>, label: &str, verbosity: Verbosity) {
    if verbosity == Verbosity::Quiet {
        return;
    }
    let style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlue)));
    let _ = writeln!(
        w,
        "{s}▶ scanning {label}{r}",
        s = style.render(),
        r = Reset.render(),
        label = label,
    );
}

pub fn render_batch_header(w: &mut AutoStream<std::io::Stdout>, paths: usize, verbosity: Verbosity) {
    if verbosity == Verbosity::Quiet {
        return;
    }
    let style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlue)));
    let _ = writeln!(
        w,
        "{s}▶ batch — {paths} {label}{r}",
        s = style.render(),
        r = Reset.render(),
        paths = paths,
        label = if paths == 1 { "path" } else { "paths" },
    );
}

/// Sanitize a repo-relative path for single-line terminal output: collapse
/// newlines and strip the remaining control characters (including ANSI escape
/// introducers) so a crafted filename can't inject escape sequences or break the
/// one-line-per-file layout. Mirrors the newline-flattening done in
/// `cli::render::truncate`, but also drops control chars since paths are
/// rendered raw (not length-capped).
fn sanitize_path(path: &str) -> String {
    path.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

fn format_line(res: &FileResult, verbosity: Verbosity) -> Option<String> {
    let row = row_for(res, verbosity)?;
    let style = row.style.render();
    let reset = Reset.render();
    let symbol = row.symbol;
    let label = row.label;
    let path = sanitize_path(&res.path);
    let detail = row.detail;
    let detail_block = if detail.is_empty() {
        String::new()
    } else {
        let dim = Style::new().dimmed().render();
        format!(" {dim}{detail}{reset}")
    };
    Some(format!("{style}{symbol} {label:<5}{reset} {path}{detail_block}",))
}

struct Row<'a> {
    symbol: char,
    label: &'a str,
    style: Style,
    detail: String,
}

fn row_for(res: &FileResult, verbosity: Verbosity) -> Option<Row<'_>> {
    let v = verbosity;
    match &res.status {
        FileStatus::Updated { had_errors: false, .. } => {
            if v == Verbosity::Verbose {
                Some(Row {
                    symbol: '✓',
                    label: "ok",
                    style: Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))),
                    detail: String::new(),
                })
            } else {
                None
            }
        }
        FileStatus::Updated {
            had_errors: true,
            error_count,
        } => {
            if v == Verbosity::Quiet {
                None
            } else {
                Some(Row {
                    symbol: '⚠',
                    label: "warn",
                    style: Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
                    detail: format!(
                        "({n} parse error{s}, partial map)",
                        n = error_count,
                        s = if *error_count == 1 { "" } else { "s" }
                    ),
                })
            }
        }
        FileStatus::Unchanged => {
            if v == Verbosity::Verbose {
                Some(Row {
                    symbol: '·',
                    label: "same",
                    style: Style::new().dimmed(),
                    detail: String::new(),
                })
            } else {
                None
            }
        }
        FileStatus::Removed => {
            if v == Verbosity::Quiet {
                None
            } else {
                Some(Row {
                    symbol: '×',
                    label: "gone",
                    style: Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
                    detail: String::new(),
                })
            }
        }
        FileStatus::SkippedTooLarge { size } => {
            if v == Verbosity::Verbose {
                Some(Row {
                    symbol: '·',
                    label: "skip",
                    style: Style::new().dimmed(),
                    detail: format!("({} too large)", human_bytes(*size)),
                })
            } else {
                None
            }
        }
        FileStatus::SkippedNonUtf8 => {
            if v == Verbosity::Verbose {
                Some(Row {
                    symbol: '·',
                    label: "skip",
                    style: Style::new().dimmed(),
                    detail: "(non-UTF8)".to_string(),
                })
            } else {
                None
            }
        }
        FileStatus::SkippedNoLang => {
            if v == Verbosity::Verbose {
                Some(Row {
                    symbol: '·',
                    label: "skip",
                    style: Style::new().dimmed(),
                    detail: "(no language)".to_string(),
                })
            } else {
                None
            }
        }
        FileStatus::SkippedBinary => {
            if v == Verbosity::Verbose {
                Some(Row {
                    symbol: '·',
                    label: "skip",
                    style: Style::new().dimmed(),
                    detail: "(binary)".to_string(),
                })
            } else {
                None
            }
        }
        FileStatus::ParseTimedOut => Some(Row {
            symbol: '✗',
            label: "fail",
            style: Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
            detail: "(parse timed out — file likely pathological)".to_string(),
        }),
        FileStatus::ReadFailed { msg, .. } => Some(Row {
            symbol: '✗',
            label: "fail",
            style: Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
            detail: format!("(read failed: {msg})"),
        }),
        FileStatus::ExtractFailed { msg } => Some(Row {
            symbol: '✗',
            label: "fail",
            style: Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
            detail: format!("(extract failed: {msg})"),
        }),
        #[cfg(feature = "documents")]
        FileStatus::DocIndexed {
            chunk_count,
            embedding_dim,
        } => {
            if v == Verbosity::Quiet {
                None
            } else {
                Some(Row {
                    symbol: '✓',
                    label: "doc",
                    style: Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlue))),
                    detail: format!("({chunk_count} chunks, dim={embedding_dim})"),
                })
            }
        }
    }
}

fn human_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b >= GB {
        format!("{:.1}GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.1}MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1}KB", b as f64 / KB as f64)
    } else {
        format!("{b}B")
    }
}

// Silence unused warnings for COL_PATH which is documentation rather than logic.
const _: usize = COL_PATH;

#[cfg(test)]
mod tests {
    use super::sanitize_path;

    #[test]
    fn sanitize_path_flattens_newlines_to_spaces() {
        assert_eq!(sanitize_path("src/a\nb.rs"), "src/a b.rs");
        assert_eq!(sanitize_path("src/a\r\nb.rs"), "src/a  b.rs");
    }

    #[test]
    fn sanitize_path_strips_ansi_and_control_chars() {
        // ESC ([1b) + a bell + a tab should all be removed; printable text stays.
        let crafted = "src/\x1b[31mevil\x07\tname.rs";
        assert_eq!(sanitize_path(crafted), "src/[31mevilname.rs");
    }

    #[test]
    fn sanitize_path_leaves_plain_paths_unchanged() {
        assert_eq!(sanitize_path("src/render.rs"), "src/render.rs");
    }
}
