//! Agent-comms CLI verbs (`basemind comms <verb>`).
//!
//! Unlike the code-map / memory CLI groups — which build a one-shot
//! [`BasemindServer`](crate::mcp::BasemindServer) and call the identical `#[tool]` an MCP client
//! would — the comms verbs connect to the user-global broker daemon DIRECTLY via
//! [`CommsClient::ensure_and_connect`]. Building a full server here would take the repo index
//! lock and clash with a running `basemind serve`; the daemon is a separate process, so a thin
//! client is both correct and lock-free.
//!
//! Human output for `history` / `inbox` prints the front-matter table (subject, from, ts, id)
//! and never bodies; `read <message_id>` is the only verb that prints a body. `--json` emits
//! the structured response for every verb.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;

use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::cursor::Cursor;
use crate::comms::ids::{AgentId, RoomId};
use crate::comms::model::{AgentCard, Room, RoomScope};
use crate::comms::protocol::SeqMeta;

/// Default page size for `history` / `inbox` when `--limit` is omitted.
const DEFAULT_LIMIT: u32 = 100;
/// Hard page cap, mirroring the broker's `MAX_LIMIT`.
const MAX_LIMIT: u32 = 1000;

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
    },
    /// List agents known to the broker, optionally restricted to one room.
    Agents {
        /// Restrict to subscribers of this room.
        #[arg(long)]
        room: Option<String>,
    },
    /// Create (and register) a room with an explicit scope.
    RoomCreate {
        /// Room id to create.
        room: String,
        /// Scope: `global` (default), `remote:<url>`, or `path:<dir>`.
        #[arg(long, default_value = "global")]
        scope: String,
        /// Human-readable title.
        #[arg(long)]
        title: Option<String>,
    },
    /// List rooms whose scope matches this repo (git remote + cwd).
    Rooms,
    /// Subscribe this agent to a room.
    Join {
        /// Room to join.
        room: String,
    },
    /// Unsubscribe this agent from a room.
    Leave {
        /// Room to leave.
        room: String,
    },
    /// Post a message to a room.
    Post {
        /// Target room.
        room: String,
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
    },
    /// Read a room's history (front-matter only; bodies via `read`).
    History {
        /// Room to read.
        room: String,
        /// Resume token from a previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum messages to return (default 100, max 1000).
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Print a single message BODY by id (the only body path).
    Read {
        /// Message id (the `id` of a front-matter row).
        message_id: String,
    },
    /// Read this agent's inbox across subscribed rooms (front-matter only).
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
    },
}

/// Parse a `--scope` string into a [`RoomScope`]: `global`, `remote:<url>`, or `path:<dir>`.
fn parse_scope(raw: &str) -> Result<RoomScope> {
    if raw == "global" {
        return Ok(RoomScope::Global);
    }
    if let Some(url) = raw.strip_prefix("remote:") {
        return Ok(RoomScope::Remote(url.to_string()));
    }
    if let Some(path) = raw.strip_prefix("path:") {
        return Ok(RoomScope::PathPrefix(std::path::PathBuf::from(path)));
    }
    anyhow::bail!("invalid --scope {raw:?}: expected `global`, `remote:<url>`, or `path:<dir>`")
}

/// Clamp a caller limit to `[1, MAX_LIMIT]`, defaulting when absent.
fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Resolve the CLI agent identity, tiered to MATCH the `serve` resolver so CLI-driven comms (the
/// notification hooks + the background monitor) share the session's identity — without that, a
/// poller would see the session's own posts as unread (server-side self-exclusion keys on the
/// requesting agent id).
///
/// 1. `BASEMIND_AGENT_ID` env — explicit override.
/// 2. The persisted `<root>/.basemind/agent-id` written by `serve` — read-only here (the CLI never
///    mints a new identity), so polls from the same repo resolve to the running session's id.
/// 3. `basemind-cli` — fixed fallback when no session has run in this repo.
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

/// Dispatch one comms agent verb. Builds a small current-thread runtime, connects a
/// [`CommsClient`] directly (spawning the daemon on first use), runs the verb, and renders.
pub fn run(root: &Path, json: bool, cmd: CommsAgentCmd) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        let agent = cli_agent_id(root)?;
        let (remote, cwd) = scope_context_for(root);
        let mut client = CommsClient::ensure_and_connect(agent, remote.clone(), cwd.clone())
            .await
            .map_err(|e| anyhow::anyhow!("connect to comms daemon: {e}"))?;
        let mut out = std::io::stdout().lock();
        dispatch(&mut client, root, json, cmd, &mut out).await
    })
}

/// Run the verb against a connected client and render to `out`.
async fn dispatch(
    client: &mut CommsClient,
    root: &Path,
    json: bool,
    cmd: CommsAgentCmd,
    out: &mut impl Write,
) -> Result<()> {
    match cmd {
        CommsAgentCmd::Register {
            name,
            description,
            version,
            skills,
        } => {
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
                writeln!(
                    out,
                    "{}",
                    json!({ "agent_id": agent_id, "registered": true })
                )?;
            } else {
                writeln!(out, "registered as {agent_id}")?;
            }
        }
        CommsAgentCmd::Agents { room } => {
            let room = room.map(RoomId::parse).transpose().context("room id")?;
            let agents = client
                .list_agents(room)
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
                    writeln!(
                        out,
                        "{}\t{}\t{}",
                        a.agent_id.as_str(),
                        a.card.name,
                        a.card.version
                    )?;
                }
            }
        }
        CommsAgentCmd::RoomCreate { room, scope, title } => {
            let room_id = RoomId::parse(room).context("room id")?;
            let scope = parse_scope(&scope)?;
            let created = client
                .create_room(room_id, scope, title)
                .await
                .map_err(|e| anyhow::anyhow!("create room: {e}"))?;
            render_room(&created, json, out)?;
        }
        CommsAgentCmd::Rooms => {
            let (remote, cwd) = scope_context_for(root);
            let rooms = client
                .list_rooms(remote, cwd)
                .await
                .map_err(|e| anyhow::anyhow!("list rooms: {e}"))?;
            if json {
                let rows: Vec<_> = rooms.iter().map(room_json).collect();
                writeln!(out, "{}", json!({ "total": rows.len(), "rooms": rows }))?;
            } else if rooms.is_empty() {
                writeln!(out, "no rooms")?;
            } else {
                for r in &rooms {
                    writeln!(out, "{}\t{}", r.room_id.as_str(), r.title)?;
                }
            }
        }
        CommsAgentCmd::Join { room } => {
            let room_id = RoomId::parse(room).context("room id")?;
            let label = room_id.as_str().to_string();
            client
                .join_room(room_id)
                .await
                .map_err(|e| anyhow::anyhow!("join: {e}"))?;
            if json {
                writeln!(out, "{}", json!({ "room": label, "joined": true }))?;
            } else {
                writeln!(out, "joined {label}")?;
            }
        }
        CommsAgentCmd::Leave { room } => {
            let room_id = RoomId::parse(room).context("room id")?;
            let label = room_id.as_str().to_string();
            client
                .leave_room(room_id)
                .await
                .map_err(|e| anyhow::anyhow!("leave: {e}"))?;
            if json {
                writeln!(out, "{}", json!({ "room": label, "left": true }))?;
            } else {
                writeln!(out, "left {label}")?;
            }
        }
        CommsAgentCmd::Post {
            room,
            subject,
            body,
            tags,
            reply_to,
        } => {
            let room_id = RoomId::parse(room).context("room id")?;
            let body = body.unwrap_or_default().into_bytes();
            let message_id = client
                .post_message(room_id, subject, body, tags, reply_to, Vec::new())
                .await
                .map_err(|e| anyhow::anyhow!("post: {e}"))?;
            if json {
                writeln!(out, "{}", json!({ "message_id": message_id }))?;
            } else {
                writeln!(out, "{message_id}")?;
            }
        }
        CommsAgentCmd::History {
            room,
            cursor,
            limit,
        } => {
            let room_id = RoomId::parse(room).context("room id")?;
            let (messages, next_cursor) = client
                .read_history(room_id, cursor.map(Cursor), clamp_limit(limit))
                .await
                .map_err(|e| anyhow::anyhow!("history: {e}"))?;
            render_front_matter(&messages, next_cursor.as_ref(), None, json, out)?;
        }
        CommsAgentCmd::Read { message_id } => {
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
        } => {
            let (remote, cwd) = scope_context_for(root);
            let (messages, unread, next_cursor) = client
                .read_inbox(
                    remote,
                    cwd,
                    cursor.map(Cursor),
                    clamp_limit(limit),
                    mark_read,
                )
                .await
                .map_err(|e| anyhow::anyhow!("inbox: {e}"))?;
            render_front_matter(&messages, next_cursor.as_ref(), Some(unread), json, out)?;
        }
    }
    Ok(())
}

/// JSON view of a room front-matter row.
fn room_json(room: &Room) -> serde_json::Value {
    json!({
        "room_id": room.room_id.as_str(),
        "title": room.title,
        "created_at": room.created_at,
    })
}

/// Render a single room (created / fetched).
fn render_room(room: &Room, json: bool, out: &mut impl Write) -> Result<()> {
    if json {
        writeln!(out, "{}", json!({ "room": room_json(room) }))?;
    } else {
        writeln!(out, "{}\t{}", room.room_id.as_str(), room.title)?;
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
                    "room": m.room.as_str(),
                    "from": m.from.as_str(),
                    "ts_micros": m.ts_micros,
                    "subject": m.subject,
                    "tags": m.tags,
                    "scope": m.scope,
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
        // Front-matter table: subject, from, ts, id — bodies are fetched with `read`.
        for sm in messages {
            let m = &sm.meta;
            writeln!(
                out,
                "{}\t{}\t{}\t{}",
                m.subject,
                m.from.as_str(),
                m.ts_micros,
                m.id
            )?;
        }
    }
    if let Some(c) = next_cursor {
        writeln!(out, "next_cursor: {}", c.0)?;
    }
    Ok(())
}
