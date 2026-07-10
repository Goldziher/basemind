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
//!
//! Multi-identity: the identity-bearing verbs accept `--as-agent <AGENT_ID>` to connect to the
//! broker AS a named sub-identity instead of the CLI's default (`cli_agent_id`). Because each
//! invocation is a ONE-SHOT process with ONE connection, "act as X" is simply "connect as X". The
//! `dm` verb delivers a direct message to one agent's inbox via a private pairwise room
//! (`dm:<lo>:<hi>`), hosting BOTH the sender's and the recipient's broker connections sequentially
//! within the single process — the same trick the MCP registry uses in `run_dm_send`.

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
/// Default recency window for `history` / `inbox` when `--since-hours` is omitted: only the last
/// 24 hours of messages are returned. Pass `--since-hours 0` for the full log.
const DEFAULT_SINCE_HOURS: u32 = 24;
/// Microseconds in one hour — scale factor for `--since-hours` → absolute `since_micros` cutoff.
const MICROS_PER_HOUR: i64 = 3_600_000_000;
/// Room-freshness window in hours: a room whose last post is older than this — or which has never
/// had a post — renders as STALE in `rooms`. 168h = 7 days, matching the MCP `RoomSummary` rule.
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
    /// List agents known to the broker, optionally restricted to one room.
    Agents {
        /// Restrict to subscribers of this room.
        #[arg(long)]
        room: Option<String>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
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
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// List rooms whose scope matches this repo (git remote + cwd).
    Rooms,
    /// Resolve the repo at a path to its canonical room (by git remote, else repo path) and join
    /// it — get-or-create. Lets an agent coordinate in ANOTHER repo's room.
    RoomForPath {
        /// Filesystem path inside (or naming) the target repo. Resolved to the repo root so any
        /// subdirectory maps to a single room.
        path: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Subscribe this agent to a room.
    Join {
        /// Room to join.
        room: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Unsubscribe this agent from a room.
    Leave {
        /// Room to leave.
        room: String,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
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
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
    },
    /// Send a direct message to one agent's inbox via a private pairwise room.
    ///
    /// Delivery is via a canonical `dm:<lo>:<hi>` room (the two agent ids sorted), scoped so the
    /// broker auto-joins NOBODY — only the sender and recipient are subscribed. The message then
    /// surfaces in the recipient's `inbox` like any other subscribed-room post.
    Dm {
        /// Recipient agent id (the DM lands in this agent's inbox).
        #[arg(long)]
        to: String,
        /// Subject line.
        #[arg(long)]
        subject: String,
        /// Message body (markdown). Empty when omitted.
        #[arg(long)]
        body: Option<String>,
        /// Id of the message being replied to.
        #[arg(long)]
        reply_to: Option<String>,
        /// Act as this sub-identity (the sender) instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
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
        /// Only return messages from the last N hours (default 24). Pass 0 for ALL history.
        #[arg(long)]
        since_hours: Option<u32>,
        /// Act as this sub-identity instead of the CLI's default agent id.
        #[arg(long)]
        as_agent: Option<String>,
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

/// Translate `--since-hours` into the absolute `since_micros` cutoff the broker filters on. `None`
/// ⇒ the [`DEFAULT_SINCE_HOURS`] default; `Some(0)` ⇒ `None` (all history); otherwise `now - hours`.
fn since_cutoff(since_hours: Option<u32>) -> Option<i64> {
    let hours = since_hours.unwrap_or(DEFAULT_SINCE_HOURS);
    if hours == 0 {
        None
    } else {
        Some(crate::comms::model::now_micros() - i64::from(hours) * MICROS_PER_HOUR)
    }
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

/// Dispatch one comms agent verb. Builds a small current-thread runtime, then runs the verb —
/// each verb connects its own [`CommsClient`] (spawning the daemon on first use) AS the resolved
/// identity, so a `--as-agent` override applies per-verb.
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

/// Connect a [`CommsClient`] to the broker as a resolved identity: the `--as-agent` override
/// (validated through [`AgentId::parse`]) when supplied, otherwise the CLI's default
/// ([`cli_agent_id`]). Spawns the daemon on first use. Threading the override through this single
/// helper keeps every verb's identity resolution DRY.
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
        CommsAgentCmd::Agents { room, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
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
                    writeln!(out, "{}\t{}\t{}", a.agent_id.as_str(), a.card.name, a.card.version)?;
                }
            }
        }
        CommsAgentCmd::RoomCreate {
            room,
            scope,
            title,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let room_id = RoomId::parse(room).context("room id")?;
            let scope = parse_scope(&scope)?;
            let created = client
                .create_room(room_id, scope, title)
                .await
                .map_err(|e| anyhow::anyhow!("create room: {e}"))?;
            render_room(&created, json, out)?;
        }
        CommsAgentCmd::Rooms => {
            let mut client = connect_as(root, None).await?;
            let (remote, cwd) = scope_context_for(root);
            let rooms = client
                .list_rooms(remote, cwd)
                .await
                .map_err(|e| anyhow::anyhow!("list rooms: {e}"))?;
            let now = crate::comms::model::now_micros();
            if json {
                let rows: Vec<_> = rooms.iter().map(|r| room_json(r, now)).collect();
                writeln!(out, "{}", json!({ "total": rows.len(), "rooms": rows }))?;
            } else if rooms.is_empty() {
                writeln!(out, "no rooms")?;
            } else {
                for r in &rooms {
                    let marker = if is_stale(r, now) { "STALE" } else { "ACTIVE" };
                    writeln!(
                        out,
                        "{}\t{}\t{}\t{}",
                        r.room_id.as_str(),
                        r.title,
                        r.last_activity,
                        marker
                    )?;
                }
            }
        }
        CommsAgentCmd::RoomForPath { path, as_agent } => {
            room_for_path(root, json, path, as_agent, out).await?;
        }
        CommsAgentCmd::Join { room, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
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
        CommsAgentCmd::Leave { room, as_agent } => {
            let mut client = connect_as(root, as_agent).await?;
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
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
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
        CommsAgentCmd::Dm {
            to,
            subject,
            body,
            reply_to,
            as_agent,
        } => {
            dm(root, json, to, subject, body, reply_to, as_agent, out).await?;
        }
        CommsAgentCmd::History {
            room,
            cursor,
            limit,
            since_hours,
            as_agent,
        } => {
            let mut client = connect_as(root, as_agent).await?;
            let room_id = RoomId::parse(room).context("room id")?;
            let (messages, next_cursor) = client
                .read_history(
                    room_id,
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

/// Send a direct message to one agent's inbox via a private pairwise room, mirroring the MCP
/// `run_dm_send` semantics for the one-shot CLI: derive the canonical `dm:<lo>:<hi>` room (the two
/// ids sorted), connect as the SENDER to create + join + post, then host a SECOND connection as the
/// RECIPIENT to join — so the DM lands in the recipient's inbox. The room is scoped to a unique
/// `Session("dm:<lo>:<hi>")` token that auto-joins nobody; membership is explicit on both ends.
#[allow(clippy::too_many_arguments)]
async fn dm(
    root: &Path,
    json: bool,
    to: String,
    subject: String,
    body: Option<String>,
    reply_to: Option<String>,
    as_agent: Option<String>,
    out: &mut impl Write,
) -> Result<()> {
    let from_agent = match &as_agent {
        Some(raw) => AgentId::parse(raw.clone()).with_context(|| format!("invalid --as-agent {raw:?}"))?,
        None => cli_agent_id(root)?,
    };
    let to_agent = AgentId::parse(to.clone()).with_context(|| format!("invalid --to {to:?}"))?;
    if from_agent == to_agent {
        anyhow::bail!("cannot dm yourself");
    }

    let (lo, hi) = if from_agent.as_str() <= to_agent.as_str() {
        (from_agent.as_str(), to_agent.as_str())
    } else {
        (to_agent.as_str(), from_agent.as_str())
    };
    let room = RoomId::parse(format!("dm:{lo}:{hi}")).context("derive dm room id")?;
    let dm_scope = RoomScope::Session(format!("dm:{lo}:{hi}"));
    let title = format!("dm {lo} <-> {hi}");

    let mut sender = connect_as(root, as_agent).await?;
    sender
        .create_room(room.clone(), dm_scope, Some(title))
        .await
        .map_err(|e| anyhow::anyhow!("create dm room: {e}"))?;
    sender
        .join_room(room.clone())
        .await
        .map_err(|e| anyhow::anyhow!("sender join: {e}"))?;

    let mut recipient = connect_as(root, Some(to_agent.as_str().to_string())).await?;
    recipient
        .join_room(room.clone())
        .await
        .map_err(|e| anyhow::anyhow!("recipient join: {e}"))?;

    let body = body.unwrap_or_default().into_bytes();
    let message_id = sender
        .post_message(room.clone(), subject, body, Vec::new(), reply_to, Vec::new())
        .await
        .map_err(|e| anyhow::anyhow!("dm post: {e}"))?;

    let room_label = room.into_string();
    if json {
        writeln!(out, "{}", json!({ "message_id": message_id, "room": room_label }))?;
    } else {
        writeln!(out, "{message_id}\t{room_label}")?;
    }
    Ok(())
}

/// Resolve the repo at `path` to its canonical room — keyed by git remote when present, else the
/// repo root path — and join it, mirroring the MCP `run_get_or_create_chat_room_for_path` body.
/// `path` is resolved to the repo ROOT so any subdirectory maps to one room; the room id / scope /
/// title come from [`repo_room_for`](crate::comms::daemon::repo_room_for), the SAME derivation the
/// broker's auto-join uses.
async fn room_for_path(
    root: &Path,
    json: bool,
    path: String,
    as_agent: Option<String>,
    out: &mut impl Write,
) -> Result<()> {
    let base = crate::git::Repo::discover(Path::new(&path))
        .ok()
        .map(|r| r.workdir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(&path));
    let (remote, cwd) = scope_context_for(&base);
    let room = crate::comms::daemon::repo_room_for(remote, cwd);
    let scope_label = match &room.scope {
        RoomScope::Remote(_) => "remote",
        RoomScope::PathPrefix(_) => "path",
        RoomScope::Session(_) => "session",
        RoomScope::Global => "global",
    };

    let mut client = connect_as(root, as_agent).await?;
    client
        .create_room(room.room_id.clone(), room.scope.clone(), Some(room.title.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("create room: {e}"))?;
    client
        .join_room(room.room_id.clone())
        .await
        .map_err(|e| anyhow::anyhow!("join: {e}"))?;

    let room_label = room.room_id.as_str().to_string();
    if json {
        writeln!(
            out,
            "{}",
            json!({ "room": room_label, "scope": scope_label, "title": room.title })
        )?;
    } else {
        writeln!(out, "{room_label}\t{scope_label}")?;
    }
    Ok(())
}

/// Whether a room reads as STALE at `now_micros`: no posts yet (`last_activity == 0`) or its last
/// post is older than the [`STALE_AFTER_HOURS`] window. Mirrors the MCP `RoomSummary` rule.
fn is_stale(room: &Room, now_micros: i64) -> bool {
    room.last_activity == 0 || (now_micros - room.last_activity) > STALE_AFTER_HOURS * MICROS_PER_HOUR
}

/// JSON view of a room front-matter row, including freshness (`last_activity` + `stale`).
fn room_json(room: &Room, now_micros: i64) -> serde_json::Value {
    json!({
        "room_id": room.room_id.as_str(),
        "title": room.title,
        "created_at": room.created_at,
        "last_activity": room.last_activity,
        "stale": is_stale(room, now_micros),
    })
}

/// Render a single room (created / fetched).
fn render_room(room: &Room, json: bool, out: &mut impl Write) -> Result<()> {
    if json {
        writeln!(
            out,
            "{}",
            json!({ "room": room_json(room, crate::comms::model::now_micros()) })
        )?;
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
