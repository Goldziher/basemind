//! `basemind sessions`: tool-use counts from the local Claude Code and Codex transcripts of this
//! repository and its worktrees (issue #128, ADR-0049).
//!
//! It runs in this process, never through the daemon: it reads the operator's home, so no relayed
//! call may reach it, and it has no MCP tool. It prints counts and repository-relative paths to
//! stdout and writes nothing else.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;

use crate::sessions::tally::{Counts, REPEATED_READ_THRESHOLD};
use crate::sessions::{Report, Scope, SessionRoots, collect};

#[derive(Args, Debug)]
pub struct SessionsArgs {
    /// Only sessions active on or after this day (YYYY-MM-DD, UTC).
    #[arg(long, value_parser = parse_since)]
    pub since: Option<String>,
}

fn parse_since(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let shape = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    let month = value.get(5..7).and_then(|m| m.parse::<u8>().ok());
    let day = value.get(8..10).and_then(|d| d.parse::<u8>().ok());
    if shape && month.is_some_and(|m| (1..=12).contains(&m)) && day.is_some_and(|d| (1..=31).contains(&d)) {
        Ok(value.to_owned())
    } else {
        Err(format!("expected a date as YYYY-MM-DD, got `{value}`"))
    }
}

/// Run `basemind sessions` for the repository at `root`, reading the roots under the home directory.
pub fn run(root: &Path, json: bool, args: &SessionsArgs) -> Result<()> {
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("no home directory to read session transcripts from")?;
    let report = collect(
        &SessionRoots::under_home(&home),
        &Scope::discover(root),
        args.since.as_deref(),
    );
    let mut out = std::io::stdout().lock();
    render(&report, json, &mut out)
}

/// Write `report` as JSON or as text.
pub fn render(report: &Report, json: bool, out: &mut impl Write) -> Result<()> {
    if json {
        let text = serde_json::to_string_pretty(report).context("serialize the sessions report")?;
        writeln!(out, "{text}")?;
        return Ok(());
    }
    for root in &report.roots {
        writeln!(out, "{} transcripts: {}", root.host.as_str(), root.state)?;
    }
    if report.sessions.is_empty() {
        writeln!(out, "no sessions of this repository")?;
    } else {
        writeln!(
            out,
            "{:<6} {:<40} {:<16} {:<10} {:>4} {:>5} {:>5} {:>4} {:>5} {:>6} {:>4} {:>5} {:>4} {:>8}",
            "host",
            "session",
            "checkout",
            "ended",
            "bm",
            "tool",
            "shell",
            "git",
            "whole",
            "ranged",
            "edit",
            "write",
            "err",
            "adoption"
        )?;
        for session in &report.sessions {
            let ended = session.ended.as_deref().and_then(|e| e.get(..10)).unwrap_or("-");
            row(
                out,
                session.host.as_str(),
                &session.id,
                &session.checkout,
                ended,
                &session.counts,
            )?;
        }
        let aggregate = &report.aggregate;
        row(
            out,
            "all",
            &format!("{} sessions", aggregate.sessions),
            "",
            "",
            &aggregate.counts,
        )?;
        let c = &aggregate.counts;
        writeln!(
            out,
            "edits without a prior read: {}  writes before any search: {}  files read {}+ times: {}  outside paths: {}",
            c.edits_without_read,
            c.writes_without_search,
            REPEATED_READ_THRESHOLD,
            c.files_read_repeatedly,
            c.outside_paths
        )?;
        writeln!(
            out,
            "output tokens: read/search {}  edit {}  other {}",
            c.output_tokens.read_search, c.output_tokens.edit, c.output_tokens.other
        )?;
        let repeated: Vec<_> = report
            .sessions
            .iter()
            .flat_map(|s| s.repeated_reads.iter().map(move |r| (&s.id, r)))
            .collect();
        if !repeated.is_empty() {
            writeln!(out, "repeated reads:")?;
            for (id, read) in repeated {
                writeln!(out, "  {id}  {}  x{}", read.path, read.reads)?;
            }
        }
    }
    for unsupported in &report.unsupported {
        writeln!(
            out,
            "unsupported: {} {} ({})",
            unsupported.host.as_str(),
            unsupported.id,
            unsupported.reason
        )?;
    }
    let skipped = [
        (report.unsupported_unattributed, "unsupported with no working directory"),
        (report.other_directories, "of other directories"),
        (report.empty, "empty"),
        (report.refused, "refused (symlink or outside the root)"),
        (report.unreadable, "unreadable"),
    ];
    for (count, what) in skipped {
        if count > 0 {
            writeln!(out, "skipped {count} transcripts: {what}")?;
        }
    }
    Ok(())
}

fn row(out: &mut impl Write, host: &str, id: &str, checkout: &str, ended: &str, c: &Counts) -> Result<()> {
    let adoption = c.adoption().map_or_else(|| "-".to_owned(), |a| format!("{a:.2}"));
    writeln!(
        out,
        "{host:<6} {id:<40} {checkout:<16} {ended:<10} {:>4} {:>5} {:>5} {:>4} {:>5} {:>6} {:>4} {:>5} {:>4} {adoption:>8}",
        c.basemind, c.search_tool, c.shell_search, c.shell_git, c.read_whole, c.read_range, c.edit, c.write, c.errors
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_takes_a_calendar_day_only() {
        assert_eq!(parse_since("2026-11-01").as_deref(), Ok("2026-11-01"));
        assert!(parse_since("2026-13-01").is_err());
        assert!(parse_since("7d").is_err());
        assert!(parse_since("2026-1-01").is_err());
    }
}
