//! Agent-comms CLI verbs (`basemind comms <verb>`).
//!
//! Unlike the code-map / memory CLI groups — which build a one-shot
//! [`BasemindServer`](crate::mcp::BasemindServer) and call the identical `#[tool]` an MCP client
//! would — the comms verbs connect to the user-global broker daemon DIRECTLY via
//! [`CommsClient::ensure_and_connect`]. Building a full server here would take the repo index
//! lock and clash with a running `basemind serve`; the daemon is a separate process, so a thin
//! client is both correct and lock-free.
//!
//! This is also the human-admin path: a person can inspect (`threads`, `members`, `history`) and
//! ARCHIVE any thread they created. `--json` emits the structured response for every verb.
//!
//! Multi-identity: the identity-bearing verbs accept `--as-agent <AGENT_ID>` to connect to the
//! broker AS a named sub-identity instead of the CLI's default (`cli_agent_id`).

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;

use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::cursor::Cursor;
use crate::comms::ids::{AgentId, ThreadId};
use crate::comms::model::{AgentCard, Thread};
use crate::comms::protocol::SeqMeta;

/// Default page size for `history` / `inbox` when `--limit` is omitted.
const DEFAULT_LIMIT: u32 = 100;
/// Hard page cap, mirroring the broker's `MAX_LIMIT`.
const MAX_LIMIT: u32 = 1000;
/// Default recency window for `history` / `inbox` when `--since-hours` is omitted.
const DEFAULT_SINCE_HOURS: u32 = 24;
/// Microseconds in one hour — scale factor for `--since-hours` → absolute `since_micros` cutoff.
const MICROS_PER_HOUR: i64 = 3_600_000_000;
/// Thread-freshness window in hours: 168h = 7 days, matching the MCP `ThreadSummary` rule.
const STALE_AFTER_HOURS: i64 = 168;

/// Agent-comms verbs that talk to the broker daemon directly.
#[derive(Subcommand, Debug)]
pub enum CommsAgentCmd {
    /// Register or update this agent's A2A card with the broker.
    Register {
        /// Human-readable agent name.
        #[arg(long)]
        name: Option<String>,
        /// One-line description of the agent's purpose.
        #[arg(long)]
        description: Option<String>,
        /// Agent version string.
        #[arg(long)]
        version: Option<String>,
        /// Advertised skill label (repeatable).
        #[arg(long = "skill")]
        skills: Vec<String>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// List agents known to the broker, optionally restricted to one thread's members.
    Agents {
        /// Restrict to members of this thread.
        #[arg(long)]
        thread: Option<String>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Start a thread addressed by at least two of subject / path / members.
    ThreadStart {
        /// Topic string.
        #[arg(long)]
        subject: Option<String>,
        /// Path or GLOB pattern (globset syntax) for path-based discovery.
        #[arg(long)]
        path: Option<String>,
        /// Explicit additional member agent id (repeatable; you are added automatically).
        #[arg(long = "member")]
        members: Vec<String>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// List threads discoverable to this agent (member / path-match / subject filter).
    Threads {
        /// Case-sensitive substring filter over thread subjects.
        #[arg(long)]
        subject_contains: Option<String>,
        /// Also list archived threads.
        #[arg(long)]
        include_archived: bool,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Join a thread.
    Join {
        /// Thread to join.
        thread: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Leave a thread.
    Leave {
        /// Thread to leave.
        thread: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// List the members of a thread.
    Members {
        /// Thread whose members to list.
        thread: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Add a member to a thread (creator only).
    AddMember {
        /// Thread to modify.
        thread: String,
        /// Member agent id to add.
        member: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Remove a member from a thread (creator only).
    RemoveMember {
        /// Thread to modify.
        thread: String,
        /// Member agent id to remove.
        member: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Archive a thread (creator / human-admin only).
    Archive {
        /// Thread to archive.
        thread: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Post a message to a thread.
    Post {
        /// Target thread.
        thread: String,
        /// Subject line.
        subject: String,
        /// Message body (markdown). Empty when omitted.
        #[arg(long)]
        body: Option<String>,
        /// Free-form tag (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Id of the message being replied to.
        #[arg(long)]
        reply_to: Option<String>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Read a thread's history (front-matter only; bodies via `read`).
    History {
        /// Thread to read.
        thread: String,
        /// Resume token from a previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum messages to return (default 100, max 1000).
        #[arg(long)]
        limit: Option<u32>,
        /// Only return messages from the last N hours (default 24). Pass 0 for ALL history.
        #[arg(long)]
        since_hours: Option<u32>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Print a single message BODY by id (the only body path).
    Read {
        /// Message id (the `id` of a front-matter row).
        message_id: String,
    },
    /// Read this agent's inbox across joined threads (front-matter only).
    Inbox {
        /// Resume token from a previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum messages to return (default 100, max 1000).
        #[arg(long)]
        limit: Option<u32>,
        /// Advance read cursors past the returned messages.
        #[arg(long)]
        mark_read: bool,
        /// Only return messages from the last N hours (default 24). Pass 0 for ALL history.
        #[arg(long)]
        since_hours: Option<u32>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
}

/// Clamp a caller limit to `[1, MAX_LIMIT]`, defaulting when absent.
fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Translate `--since-hours` into the absolute `since_micros` cutoff the broker filters on.
fn since_cutoff(since_hours: Option<u32>) -> Option<i64> {
    let hours = since_hours.unwrap_or(DEFAULT_SINCE_HOURS);
    if hours == 0 {
        None
    } else {
        Some(crate::comms::model::now_micros() - i64::from(hours) * MICROS_PER_HOUR)
    }
}

/// Resolve the CLI agent identity, tiered to MATCH the `serve` resolver so CLI-driven comms share
/// the session's identity.
fn cli_agent_id(root: &Path) -> Result<AgentId> {
    if let Ok(raw) = std::env::var("BASEMIND_AGENT_ID")
        && let Ok(id) = AgentId::parse(raw)
    {
        return Ok(id);
    }
    if let Ok(existing) = std::fs::read_to_string(root.join(".basemind").join("agent-id"))
        && let Ok(id) = AgentId::parse(existing.trim())
    {
        return Ok(id);
    }
    AgentId::parse("basemind-cli").context("construct CLI agent id")
}

/// Dispatch one comms agent verb. Builds a small current-thread runtime, then runs the verb.
pub fn run(root: &Path, json: bool, cmd: CommsAgentCmd) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        let mut out = std::io::stdout().lock();
        dispatch(root, json, cmd, &mut out).await
    })
}

/// Connect a [`CommsClient`] to the broker as a resolved identity.
async fn connect_as(root: &Path, as_agent: Option<String>) -> Result<CommsClient> {
    let agent = match as_agent {
        Some(raw) => AgentId::parse(raw.clone()).with_context(|| format!("invalid --as-agent {raw:?}"))?,
        None => cli_agent_id(root)?,
    };
    let (remote, cwd) = scope_context_for(root);
    CommsClient::ensure_and_connect(agent, remote, cwd)
        .await
        .map_err(|e| anyhow::anyhow!("connect to comms daemon: {e}"))
}

/// Parse the repeatable `--member` args into validated [`AgentId`]s.
fn parse_members(raw: Vec<String>) -> Result<Vec<AgentId>> {
    raw.into_iter()
        .map(|m| AgentId::parse(m.clone()).with_context(|| format!("invalid --member {m:?}")))
        .collect()
}

/// Run the verb: resolve the identity, connect, call the client method, and render to `out`.
async fn dispatch(root: &Path, json: bool, cmd: CommsAgentCmd, out: &mut impl Write) -> Result<()> {
    match cmd {
        CommsAgentCmd::Register {
            name,
            description,
            version,
            skills,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let card = AgentCard {
                name: name.unwrap_or_default(),
                description: description.unwrap_or_default(),
                version: version.unwrap_or_default(),
                skills,
            };
            let agent_id = client.agent().as_str().to_string();
            client
                .register_agent(card)
                .await
                .map_err(|e| anyhow::anyhow!("register: {e}"))?;
            if json {
                writeln!(out, "{}", json!({ "agent_id": agent_id, "registered": true }))?;
            } else {
                writeln!(out, "registered as {agent_id}")?;
            }
        }
        CommsAgentCmd::Agents { thread, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread = thread.map(ThreadId::parse).transpose().context("thread id")?;
            let agents = client
                .list_agents(thread)
                .await
                .map_err(|e| anyhow::anyhow!("list agents: {e}"))?;
            if json {
                let rows: Vec<_> = agents
                    .iter()
                    .map(|a| {
                        json!({
                            "agent_id": a.agent_id.as_str(),
                            "name": a.card.name,
                            "version": a.card.version,
                        })
                    })
                    .collect();
                writeln!(out, "{}", json!({ "total": rows.len(), "agents": rows }))?;
            } else if agents.is_empty() {
                writeln!(out, "no agents")?;
            } else {
                for a in &agents {
                    writeln!(out, "{}\t{}\t{}", a.agent_id.as_str(), a.card.name, a.card.version)?;
                }
            }
        }
        CommsAgentCmd::ThreadStart {
            subject,
            path,
            members,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let members = parse_members(members)?;
            let thread = client
                .start_thread(subject, path, members)
                .await
                .map_err(|e| anyhow::anyhow!("thread start: {e}"))?;
            render_thread(&thread, json, out)?;
        }
        CommsAgentCmd::Threads {
            subject_contains,
            include_archived,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let (remote, cwd) = scope_context_for(root);
            let threads = client
                .list_threads(remote, cwd, subject_contains, include_archived)
                .await
                .map_err(|e| anyhow::anyhow!("list threads: {e}"))?;
            let now = crate::comms::model::now_micros();
            if json {
                let rows: Vec<_> = threads.iter().map(|t| thread_json(t, now)).collect();
                writeln!(out, "{}", json!({ "total": rows.len(), "threads": rows }))?;
            } else if threads.is_empty() {
                writeln!(out, "no threads")?;
            } else {
                for t in &threads {
                    let marker = if is_stale(t, now) { "STALE" } else { "ACTIVE" };
                    let state = if t.active { marker } else { "ARCHIVED" };
                    writeln!(
                        out,
                        "{}\t{}\t{}\t{}",
                        t.id.as_str(),
                        t.subject.as_deref().unwrap_or("-"),
                        t.last_activity,
                        state
                    )?;
                }
            }
        }
        CommsAgentCmd::Join { thread, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let label = thread_id.as_str().to_string();
            client
                .join_thread(thread_id)
                .await
                .map_err(|e| anyhow::anyhow!("join: {e}"))?;
            render_flag(json, out, "thread", &label, "joined")?;
        }
        CommsAgentCmd::Leave { thread, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let label = thread_id.as_str().to_string();
            client
                .leave_thread(thread_id)
                .await
                .map_err(|e| anyhow::anyhow!("leave: {e}"))?;
            render_flag(json, out, "thread", &label, "left")?;
        }
        CommsAgentCmd::Members { thread, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let label = thread_id.as_str().to_string();
            let members = client
                .thread_members(thread_id)
                .await
                .map_err(|e| anyhow::anyhow!("members: {e}"))?;
            let ids: Vec<String> = members.iter().map(|m| m.as_str().to_string()).collect();
            if json {
                writeln!(out, "{}", json!({ "thread": label, "members": ids }))?;
            } else if ids.is_empty() {
                writeln!(out, "no members")?;
            } else {
                for m in &ids {
                    writeln!(out, "{m}")?;
                }
            }
        }
        CommsAgentCmd::AddMember {
            thread,
            member,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let member_id = AgentId::parse(member).context("member id")?;
            let label = thread_id.as_str().to_string();
            let member_label = member_id.as_str().to_string();
            client
                .add_member(thread_id, member_id)
                .await
                .map_err(|e| anyhow::anyhow!("add member: {e}"))?;
            if json {
                writeln!(
                    out,
                    "{}",
                    json!({ "thread": label, "member": member_label, "added": true })
                )?;
            } else {
                writeln!(out, "added {member_label} to {label}")?;
            }
        }
        CommsAgentCmd::RemoveMember {
            thread,
            member,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let member_id = AgentId::parse(member).context("member id")?;
            let label = thread_id.as_str().to_string();
            let member_label = member_id.as_str().to_string();
            client
                .remove_member(thread_id, member_id)
                .await
                .map_err(|e| anyhow::anyhow!("remove member: {e}"))?;
            if json {
                writeln!(
                    out,
                    "{}",
                    json!({ "thread": label, "member": member_label, "removed": true })
                )?;
            } else {
                writeln!(out, "removed {member_label} from {label}")?;
            }
        }
        CommsAgentCmd::Archive { thread, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let label = thread_id.as_str().to_string();
            client
                .archive_thread(thread_id)
                .await
                .map_err(|e| anyhow::anyhow!("archive: {e}"))?;
            if json {
                writeln!(out, "{}", json!({ "thread": label, "archived": true }))?;
            } else {
                writeln!(out, "archived {label}")?;
            }
        }
        CommsAgentCmd::Post {
            thread,
            subject,
            body,
            tags,
            reply_to,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let body = body.unwrap_or_default().into_bytes();
            let message_id = client
                .post_message(thread_id, subject, body, tags, reply_to)
                .await
                .map_err(|e| anyhow::anyhow!("post: {e}"))?;
            if json {
                writeln!(out, "{}", json!({ "message_id": message_id }))?;
            } else {
                writeln!(out, "{message_id}")?;
            }
        }
        CommsAgentCmd::History {
            thread,
            cursor,
            limit,
            since_hours,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let thread_id = ThreadId::parse(thread).context("thread id")?;
            let (messages, next_cursor) = client
                .read_history(
                    thread_id,
                    cursor.map(Cursor),
                    clamp_limit(limit),
                    since_cutoff(since_hours),
                )
                .await
                .map_err(|e| anyhow::anyhow!("history: {e}"))?;
            render_front_matter(&messages, next_cursor.as_ref(), None, json, out)?;
        }
        CommsAgentCmd::Read { message_id } => {
            let mut client = connect_as(root, None).await?;
            let id = message_id.clone();
            let body = client
                .get_body(message_id)
                .await
                .map_err(|e| anyhow::anyhow!("read: {e}"))?;
            let text = body.map(|b| String::from_utf8_lossy(&b).into_owned());
            if json {
                writeln!(
                    out,
                    "{}",
                    json!({ "message_id": id, "found": text.is_some(), "body": text })
                )?;
            } else {
                match text {
                    Some(b) => writeln!(out, "{b}")?,
                    None => writeln!(out, "(no such message)")?,
                }
            }
        }
        CommsAgentCmd::Inbox {
            cursor,
            limit,
            mark_read,
            since_hours,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let (remote, cwd) = scope_context_for(root);
            let (messages, unread, next_cursor) = client
                .read_inbox(
                    remote,
                    cwd,
                    cursor.map(Cursor),
                    clamp_limit(limit),
                    mark_read,
                    since_cutoff(since_hours),
                )
                .await
                .map_err(|e| anyhow::anyhow!("inbox: {e}"))?;
            render_front_matter(&messages, next_cursor.as_ref(), Some(unread), json, out)?;
        }
    }
    Ok(())
}

/// Render a `{key: value, flag: true}` JSON object (or a plain line) for a membership toggle.
fn render_flag(json: bool, out: &mut impl Write, key: &str, value: &str, flag: &str) -> Result<()> {
    if json {
        writeln!(out, "{}", json!({ key: value, flag: true }))?;
    } else {
        writeln!(out, "{flag} {value}")?;
    }
    Ok(())
}

/// Whether a thread reads as STALE at `now_micros`: no posts yet or last post older than 7 days.
fn is_stale(thread: &Thread, now_micros: i64) -> bool {
    thread.last_activity == 0 || (now_micros - thread.last_activity) > STALE_AFTER_HOURS * MICROS_PER_HOUR
}

/// JSON view of a thread front-matter row.
fn thread_json(thread: &Thread, now_micros: i64) -> serde_json::Value {
    json!({
        "id": thread.id.as_str(),
        "subject": thread.subject,
        "path": thread.path,
        "members": thread.members.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
        "creator": thread.creator.as_str(),
        "active": thread.active,
        "created_at": thread.created_at,
        "last_activity": thread.last_activity,
        "stale": is_stale(thread, now_micros),
    })
}

/// Render a single thread (started / fetched).
fn render_thread(thread: &Thread, json: bool, out: &mut impl Write) -> Result<()> {
    if json {
        writeln!(
            out,
            "{}",
            json!({ "thread": thread_json(thread, crate::comms::model::now_micros()) })
        )?;
    } else {
        writeln!(
            out,
            "{}\t{}",
            thread.id.as_str(),
            thread.subject.as_deref().unwrap_or("-")
        )?;
    }
    Ok(())
}

/// Render a page of message FRONT-MATTER (never bodies). `unread` is `Some` for inbox output.
fn render_front_matter(
    messages: &[SeqMeta],
    next_cursor: Option<&Cursor>,
    unread: Option<u32>,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    if json {
        let rows: Vec<_> = messages
            .iter()
            .map(|sm| {
                let m = &sm.meta;
                json!({
                    "id": m.id,
                    "thread": m.thread.as_str(),
                    "from": m.from.as_str(),
                    "ts_micros": m.ts_micros,
                    "subject": m.subject,
                    "tags": m.tags,
                    "reply_to": m.reply_to,
                    "seq": sm.seq,
                    "body_len": m.body_len,
                })
            })
            .collect();
        let mut obj = json!({ "total": rows.len(), "messages": rows });
        if let Some(u) = unread {
            obj["unread"] = json!(u);
        }
        if let Some(c) = next_cursor {
            obj["next_cursor"] = json!(c.0);
        }
        writeln!(out, "{obj}")?;
        return Ok(());
    }
    if let Some(u) = unread {
        writeln!(out, "unread: {u}")?;
    }
    if messages.is_empty() {
        writeln!(out, "(no messages)")?;
    } else {
        for sm in messages {
            let m = &sm.meta;
            writeln!(out, "{}\t{}\t{}\t{}", m.subject, m.from.as_str(), m.ts_micros, m.id)?;
        }
    }
    if let Some(c) = next_cursor {
        writeln!(out, "next_cursor: {}", c.0)?;
    }
    Ok(())
}
