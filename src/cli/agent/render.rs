//! Output of the `basemind agent` commands.
//!
//! `--json` prints the reply as the store returned it, with every field written. The text form is
//! meant to be read and to stay stable: tab-separated header lines, one message per block, one thread per
//! line.

use std::fmt::Display;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::comms::v2::{Actor, InboxMessage, InboxReply, PostReply, ThreadSummary, ThreadsReply, WaitOutcome};

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_MINUTE: i64 = 60;
const SECS_PER_HOUR: i64 = 60 * SECS_PER_MINUTE;
const SECS_PER_DAY: i64 = 24 * SECS_PER_HOUR;

/// Separator of the ids in one line of text output.
const LIST_SEPARATOR: &str = ", ";

/// How a command renders its reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Render {
    /// Print the reply as JSON instead of text.
    pub json: bool,
    /// The time ages are counted from, in microseconds since the unix epoch.
    pub now_micros: i64,
}

impl Render {
    /// Rendering against the system clock.
    pub fn now(json: bool) -> Self {
        let now_micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(i64::MAX));
        Self { json, now_micros }
    }
}

/// What a `threads` reply is the answer to, which decides its text form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadsView {
    /// A listing: one line per thread.
    List,
    /// One thread described: its line, then its repository, creator, paths and head.
    Describe,
    /// One thread changed: the change, named by this label, then its line.
    Changed(&'static str),
}

/// How long ago `ts_micros` was at `now_micros`, in the largest whole unit: `42s`, `5m`, `3h`,
/// `2d`. A timestamp ahead of the clock reads `0s`.
pub fn age(ts_micros: i64, now_micros: i64) -> String {
    let secs = now_micros.saturating_sub(ts_micros).max(0) / MICROS_PER_SEC;
    if secs < SECS_PER_MINUTE {
        format!("{secs}s")
    } else if secs < SECS_PER_HOUR {
        format!("{}m", secs / SECS_PER_MINUTE)
    } else if secs < SECS_PER_DAY {
        format!("{}h", secs / SECS_PER_HOUR)
    } else {
        format!("{}d", secs / SECS_PER_DAY)
    }
}

/// Print `value` as pretty JSON, the rendering every `--json` command of the binary uses.
pub fn write_json(value: &impl Serialize, out: &mut dyn Write) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("serialize JSON output")?;
    writeln!(out, "{text}").context("write JSON output")?;
    Ok(())
}

fn joined<T: Display>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(LIST_SEPARATOR)
}

fn write_pending(pending: &[Actor], out: &mut dyn Write) -> Result<()> {
    if !pending.is_empty() {
        writeln!(out, "pending: {}", joined(pending))?;
    }
    Ok(())
}

fn write_notices(notices: &[String], out: &mut dyn Write) -> Result<()> {
    for notice in notices {
        writeln!(out, "notice: {notice}")?;
    }
    Ok(())
}

/// A post: the message id and whether it created its thread, then the recipients not seen yet.
pub fn post(reply: &PostReply, opts: &Render, out: &mut dyn Write) -> Result<()> {
    if opts.json {
        return write_json(reply, out);
    }
    let outcome = if reply.created { "created" } else { "posted" };
    writeln!(out, "{}\t{outcome}", reply.id)?;
    write_pending(&reply.pending, out)?;
    write_notices(&reply.notices, out)
}

/// One message: id, sender, subject and age on a line, then the body, then a blank line. A
/// message without a body, which is what an announcing read returns, shows the body's size.
fn write_message(message: &InboxMessage, now_micros: i64, out: &mut dyn Write) -> Result<()> {
    writeln!(
        out,
        "{}\t{}\t{}\t{}",
        message.id,
        message.from,
        message.subject,
        age(message.ts_micros, now_micros)
    )?;
    if let Some(reply_to) = &message.reply_to {
        writeln!(out, "reply_to: {reply_to}")?;
    }
    if message.body.is_empty() {
        writeln!(out, "({} bytes, not read yet)", message.body_bytes)?;
    } else {
        writeln!(out, "{}", message.body.trim_end_matches('\n'))?;
    }
    if message.truncated {
        writeln!(
            out,
            "(truncated: read the whole body with `basemind agent inbox --id {}`)",
            message.id
        )?;
    }
    writeln!(out)?;
    Ok(())
}

/// An inbox read: one block per message, then how to get the rest when some were left out.
pub fn inbox(reply: &InboxReply, opts: &Render, out: &mut dyn Write) -> Result<()> {
    if opts.json {
        return write_json(reply, out);
    }
    for message in &reply.messages {
        write_message(message, opts.now_micros, out)?;
    }
    if reply.messages.is_empty() {
        match reply.wait {
            WaitOutcome::TimedOut => writeln!(out, "(no messages; the wait timed out)")?,
            WaitOutcome::NotWaited | WaitOutcome::Arrived => writeln!(out, "(no messages)")?,
        }
    }
    match (&reply.next_cursor, reply.more) {
        (Some(cursor), _) => writeln!(out, "next_cursor: {cursor}")?,
        (None, true) => writeln!(out, "more: call again for the next unread messages")?,
        (None, false) => {}
    }
    write_notices(&reply.notices, out)
}

/// One thread: id, subject, state, membership, unread count, age of the last post, peers.
fn write_thread(thread: &ThreadSummary, now_micros: i64, out: &mut dyn Write) -> Result<()> {
    let access = if thread.open { "open" } else { "invite-only" };
    let archived = if thread.archived { ",archived" } else { "" };
    let membership = if thread.member { "member" } else { "not-member" };
    let peers = if thread.peers.is_empty() {
        "-".to_string()
    } else {
        joined(&thread.peers)
    };
    writeln!(
        out,
        "{}\t{}\t{access}{archived}\t{membership}\tunread {}\t{}\t{peers}",
        thread.id,
        thread.subject,
        thread.unread,
        age(thread.last_activity_micros, now_micros)
    )?;
    Ok(())
}

/// What a listing line leaves out of a thread.
fn write_thread_detail(thread: &ThreadSummary, out: &mut dyn Write) -> Result<()> {
    let paths = if thread.paths.is_empty() {
        "-".to_string()
    } else {
        joined(&thread.paths)
    };
    writeln!(out, "repo: {}", thread.repo)?;
    writeln!(out, "creator: {}", thread.creator)?;
    writeln!(out, "paths: {paths}")?;
    writeln!(out, "head: {}", thread.head)?;
    Ok(())
}

/// A thread query or change: the threads as `view` shows them, then the invited agents not seen
/// yet and the cursor of the next page.
pub fn threads(reply: &ThreadsReply, view: ThreadsView, opts: &Render, out: &mut dyn Write) -> Result<()> {
    if opts.json {
        return write_json(reply, out);
    }
    for thread in &reply.threads {
        if let ThreadsView::Changed(change) = view {
            writeln!(out, "{change} {}", thread.id)?;
        }
        write_thread(thread, opts.now_micros, out)?;
        if view == ThreadsView::Describe {
            write_thread_detail(thread, out)?;
        }
    }
    if reply.threads.is_empty() {
        writeln!(out, "(no threads)")?;
    }
    write_pending(&reply.pending, out)?;
    if let Some(cursor) = &reply.next_cursor {
        writeln!(out, "next_cursor: {cursor}")?;
    }
    write_notices(&reply.notices, out)
}

/// A report of a command with no reply type of its own: JSON as given, or one `name: value` line
/// per field.
pub fn report(value: &impl Serialize, opts: &Render, out: &mut dyn Write) -> Result<()> {
    let value = serde_json::to_value(value).context("serialize the report")?;
    if opts.json {
        return write_json(&value, out);
    }
    match value.as_object() {
        Some(fields) => {
            for (name, value) in fields {
                writeln!(out, "{name}: {value}")?;
            }
        }
        None => writeln!(out, "{value}")?,
    }
    Ok(())
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
