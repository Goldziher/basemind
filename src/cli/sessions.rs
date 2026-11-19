//! `basemind sessions`: tool-use counts from the local Claude Code and Codex transcripts of this
//! repository and its worktrees, their subagents included (issue #128, ADR-0049).
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
        header(out, "session")?;
        for session in &report.sessions {
            let ended = session.ended.as_deref().and_then(|e| e.get(..10)).unwrap_or("-");
            row(
                out,
                session.host.as_str(),
                &short(&session.id, ID_WIDTH),
                &session.checkout,
                ended,
                &session.counts,
            )?;
        }
        if !report.subagents.is_empty() {
            writeln!(out, "subagents:")?;
            header(out, "parent/subagent")?;
            for subagent in &report.subagents {
                let session = &subagent.session;
                let ended = session.ended.as_deref().and_then(|e| e.get(..10)).unwrap_or("-");
                row(
                    out,
                    session.host.as_str(),
                    &pair(&subagent.parent, &session.id),
                    &session.checkout,
                    ended,
                    &session.counts,
                )?;
            }
        }
        let aggregate = &report.aggregate;
        row(
            out,
            "all",
            &format!("{} sessions, {} subagents", aggregate.sessions, aggregate.subagents),
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
            .map(|s| (short(&s.id, ID_WIDTH), s))
            .chain(
                report
                    .subagents
                    .iter()
                    .map(|s| (pair(&s.parent, &s.session.id), &s.session)),
            )
            .flat_map(|(label, s)| s.repeated_reads.iter().map(move |r| (label.clone(), r)))
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
    if report.skipped_lines > 0 {
        writeln!(
            out,
            "skipped {} lines: not JSON of a known shape, or over {} MiB",
            report.skipped_lines,
            crate::sessions::contain::LINE_MAX >> 20
        )?;
    }
    Ok(())
}

/// The width of the id column.
const ID_WIDTH: usize = 40;

/// `id` cut to `width` characters, keeping its end (a Codex rollout's thread id) after a `~`.
fn short(id: &str, width: usize) -> String {
    let count = id.chars().count();
    if count <= width {
        return id.to_owned();
    }
    let tail: String = id.chars().skip(count - (width - 1)).collect();
    format!("~{tail}")
}

/// A subagent's `parent/id` label, each half cut so the pair fits the id column.
fn pair(parent: &str, id: &str) -> String {
    let half = (ID_WIDTH - 1) / 2;
    format!("{}/{}", short(parent, half), short(id, ID_WIDTH - 1 - half))
}

fn header(out: &mut impl Write, id: &str) -> Result<()> {
    writeln!(
        out,
        "{:<6} {:<40} {:<16} {:<10} {:>4} {:>5} {:>5} {:>4} {:>5} {:>6} {:>4} {:>5} {:>4} {:>8}",
        "host",
        id,
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

    #[test]
    fn long_ids_are_cut_to_the_column() {
        let rollout = "rollout-2026-11-18T09-20-00-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        assert_eq!(short("s1", ID_WIDTH), "s1");
        assert_eq!(short(rollout, 20), format!("~{}", &rollout[rollout.len() - 19..]));
        let label = pair(rollout, rollout);
        assert!(label.chars().count() <= ID_WIDTH, "{label}");
        assert!(label.ends_with("0c1d2e3f4a5b"), "{label}");
        assert_eq!(pair("s1", "agent-a1"), "s1/agent-a1");
    }
}
