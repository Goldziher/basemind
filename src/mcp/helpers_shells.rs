//! Helper bodies for the headless agent-shell MCP tools.
//!
//! Each helper resolves the embedded [`crate::shells::ShellRuntime`] off
//! `ServerState`, drives one rmux operation (spawn / send / capture / kill), and
//! returns a JSON [`CallToolResult`]. The whole module is gated on
//! `feature = "shells"`.

#![cfg(feature = "shells")]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::types_shells::{
    ShellBroadcastParams, ShellBroadcastResponse, ShellCaptureParams, ShellCaptureResponse,
    ShellEnv, ShellKillParams, ShellKillResponse, ShellListParams, ShellListResponse,
    ShellSendParams, ShellSessionView, ShellSpawnParams, ShellSpawnResponse,
};
use crate::shells::SessionId;
use crate::shells::session::ShellCommand;

/// Map an internal `anyhow` failure to an MCP internal error with a prefix.
fn mcp_internal(prefix: &str, err: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("{prefix}: {err}"), None)
}

/// Reconstruct a [`SessionId`] from the caller-supplied string. The id is opaque
/// to the client, so any non-empty string is accepted here and resolution
/// against the in-process map decides validity.
fn parse_session_id(raw: &str) -> Result<SessionId, McpError> {
    if raw.trim().is_empty() {
        return Err(McpError::invalid_params(
            "session_id must not be empty",
            None,
        ));
    }
    Ok(SessionId::new(raw))
}

/// Resolve a `session_id` to its rmux session name, erroring when unknown.
async fn require_session(
    state: &ServerState,
    raw: &str,
) -> Result<(SessionId, rmux_sdk::SessionName), McpError> {
    let id = parse_session_id(raw)?;
    match state.shell_runtime.resolve(&id).await {
        Some(name) => Ok((id, name)),
        None => Err(McpError::invalid_params(
            format!("unknown session_id {raw:?}; it may have been killed or never existed"),
            None,
        )),
    }
}

/// `shell_spawn`: create a detached headless shell session.
///
/// When the server is built with comms enabled (`feature = "comms"`, unix), the spawn is coupled
/// to a session-scoped comms room so the parent (this server) and the spawned child can talk
/// bidirectionally: a `RoomScope::Session(<comms_session_id>)` room is created and the parent
/// joins it BEFORE the shell starts, and the child inherits `BASEMIND_SESSION_ID` /
/// `BASEMIND_PARENT_AGENT_ID` / `BASEMIND_AGENT_ID` in its environment so its own basemind
/// auto-identifies and auto-joins the same room on its first `Hello`. The coupling is created
/// atomically before the spawn: a room-creation failure aborts the spawn, so no room-less session
/// leaks. When comms is disabled the tool behaves headless and `room_id` / `child_agent` are `None`.
pub(super) async fn run_shell_spawn(
    state: &ServerState,
    params: ShellSpawnParams,
) -> Result<CallToolResult, McpError> {
    // Honour the config master switch. `ShellsConfig::enabled` defaults to `true`, so a default
    // build behaves exactly as before; an operator who set `[shells].enabled = false` gets the
    // tool wired but inert, mirroring the documented contract on the config field.
    if !state.config.shells.enabled {
        return Err(McpError::invalid_params(
            "shells are disabled in config ([shells].enabled = false)",
            None,
        ));
    }

    // Confine the caller-supplied working directory to the repository root. `normalize_query_path`
    // rejects `..` traversal and absolute paths outside `root`, returning a clean repo-relative key
    // — so a spawned shell can never be pointed at a directory outside the indexed workspace.
    let cwd = match params.cwd {
        Some(rel) => {
            let raw = rel
                .as_str()
                .ok_or_else(|| McpError::invalid_params("cwd is not valid UTF-8", None))?;
            let normalized = crate::path::normalize_query_path(raw, &state.root)
                .ok_or_else(|| McpError::invalid_params("cwd escapes the repository root", None))?;
            Some(normalized)
        }
        None => None,
    };

    // Mint ONE session id up front: it keys the comms room the child auto-joins AND addresses the
    // rmux session for `shell_send` / `shell_capture`. Threading a single id through both paths is
    // what makes the response's `session_id` and the room the child joins provably the same value.
    let session_id = state.shell_runtime.mint_session_id();

    // Validate + sanitize the caller-supplied env BEFORE building the `KEY=VALUE` vec, so a hostile
    // entry cannot smuggle a NUL / newline / extra `KEY=VALUE` into the spawned process env.
    // `mut` is only needed when comms is on (the coupling rewrites the identity vars). The
    // attribute keeps the headless `shells`-only build free of an `unused_mut` warning.
    #[cfg_attr(not(all(feature = "comms", unix)), allow(unused_mut))]
    let mut environment = build_environment(params.env.unwrap_or_default())?;

    // Couple the session to a comms room and inject the child's identity env BEFORE the spawn, so
    // the child process starts already pointed at its room. `(None, None)` when comms is off. The
    // pre-minted `session_id` keys the room so the child joins the same one the client addresses.
    #[cfg(all(feature = "comms", unix))]
    let (room_id, child_agent) =
        couple_session_room(state, session_id.as_str(), &mut environment).await?;
    #[cfg(not(all(feature = "comms", unix)))]
    let (room_id, child_agent): (Option<String>, Option<String>) = (None, None);

    let spawned = state
        .shell_runtime
        .spawn(
            session_id.clone(),
            ShellCommand::Shell(params.command),
            cwd,
            environment,
            state.config.shells.default_cols,
            state.config.shells.default_rows,
        )
        .await;

    let (session_id, name) = match spawned {
        Ok(pair) => pair,
        Err(error) => {
            // The room was created + joined before the spawn. The spawn failed, so no child will
            // ever join it — roll the parent's subscription back so the broker room does not leak
            // (best-effort; the original spawn error is what we propagate).
            #[cfg(all(feature = "comms", unix))]
            if let Some(room) = room_id.as_deref() {
                rollback_session_room(state, room).await;
            }
            return Err(mcp_internal("spawn shell session", error));
        }
    };

    // Build the visual attach target: it re-execs basemind (no external `rmux` binary) with the
    // hidden `--__internal-attach` flag against the same socket the embedded daemon is bound to.
    let target = crate::shells::launcher::AttachTarget {
        session_name: name.as_str().to_string(),
        socket_path: state.shell_runtime.socket_path().to_path_buf(),
        cols: state.config.shells.default_cols,
        rows: state.config.shells.default_rows,
        exe: std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("basemind")),
    };
    let attach_command = target.attach_command();

    // Present the session in a terminal surface per config, BEST-EFFORT. The headless session is
    // already alive; a presentation failure (no terminal, osascript error) must not fail the spawn.
    // Skip the call entirely for Headless mode so tests (and headless operators) never spawn a
    // terminal and we avoid the no-op round trip.
    let visual = state.config.shells.visual;
    if visual != crate::config::VisualMode::Headless {
        let terminal = state.config.shells.terminal;
        if let Err(error) = crate::shells::launcher::present(visual, terminal, &target) {
            tracing::warn!(
                error = %error,
                session_id = %session_id,
                "shell_spawn: visual presentation failed; the headless session is still alive"
            );
        }
    }

    let response = ShellSpawnResponse {
        session_id: session_id.to_string(),
        attach_command,
        room_id,
        child_agent,
    };
    json_result(&response)
}

/// Loader-injection env vars worth a heads-up when a caller supplies them: they let the child
/// preload arbitrary shared objects. We warn rather than reject — a legitimate caller may need
/// them — so the spawn still proceeds.
const LOADER_VARS: [&str; 5] = [
    "LD_PRELOAD",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
];

/// Validate + sanitize the caller-supplied env entries, then render them as `KEY=VALUE` strings.
///
/// Rejects a key that is empty or contains `=` / NUL / newline / carriage return (any of which
/// would let the entry smuggle an extra variable or a control char into the process env), and a
/// value containing NUL / newline / carriage return. A loader-injection var (`LD_PRELOAD` etc.) is
/// allowed but logged at WARN.
fn build_environment(env: Vec<ShellEnv>) -> Result<Vec<String>, McpError> {
    let mut out = Vec::with_capacity(env.len());
    for kv in env {
        if kv.key.is_empty() {
            return Err(McpError::invalid_params("env key must not be empty", None));
        }
        if kv.key.contains(['=', '\0', '\n', '\r']) {
            return Err(McpError::invalid_params(
                format!(
                    "env key {:?} must not contain '=', NUL, newline, or carriage return",
                    kv.key
                ),
                None,
            ));
        }
        if kv.value.contains(['\0', '\n', '\r']) {
            return Err(McpError::invalid_params(
                format!(
                    "env value for key {:?} must not contain NUL, newline, or carriage return",
                    kv.key
                ),
                None,
            ));
        }
        if LOADER_VARS.contains(&kv.key.as_str()) {
            tracing::warn!(
                key = %kv.key,
                "shell_spawn: caller supplied a loader-injection env var; passing it through"
            );
        }
        out.push(format!("{}={}", kv.key, kv.value));
    }
    Ok(out)
}

/// Best-effort comms coupling for a spawned session. Derives the child agent from the parent +
/// the pre-minted `session_id`, creates and joins a `RoomScope::Session` room keyed by that id,
/// and injects the child's identity env into `environment` BEFORE the shell is spawned. Returns
/// `(room_id, child_agent)` on success.
///
/// The coupling is OPTIONAL: comms is an add-on, so a broker that is unreachable / down must not
/// fail the shell spawn. A comms failure is logged and the function returns `(None, None)`, leaving
/// `environment` untouched so the session spawns headless (no room, no injected identity).
///
/// The `session_id` is the single id minted by the runtime in `run_shell_spawn` and threaded here:
/// it keys the comms room the child auto-joins AND (back in the caller) addresses the rmux session,
/// so the two are provably the same value rather than two counters that happen to stay in step.
///
/// # Threat model
/// The child's identity is asserted purely through inherited `BASEMIND_*` env vars. A spawned child
/// is free to overwrite `BASEMIND_AGENT_ID` and claim another agent's id — the broker does not
/// cross-check the asserted id against the spawning parent. This is acceptable for a local
/// single-user dev tool (every process already runs as the same uid); broker-side mismatch
/// detection (warn when a child presents an id inconsistent with its `parent_agent`) is future work.
#[cfg(all(feature = "comms", unix))]
async fn couple_session_room(
    state: &ServerState,
    session_id: &str,
    environment: &mut Vec<String>,
) -> Result<(Option<String>, Option<String>), McpError> {
    match try_couple_session_room(state, session_id, environment).await {
        Ok((room_id, child_agent)) => Ok((Some(room_id), Some(child_agent))),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "shell_spawn: comms coupling unavailable; spawning the session headless"
            );
            Ok((None, None))
        }
    }
}

/// The fallible inner body of [`couple_session_room`]. On `Ok`, the room exists, the parent has
/// joined it, and the child's identity env has been appended to `environment`.
#[cfg(all(feature = "comms", unix))]
async fn try_couple_session_room(
    state: &ServerState,
    session_id: &str,
    environment: &mut Vec<String>,
) -> Result<(String, String), McpError> {
    use super::helpers_comms::{client_mut, comms_client, comms_err};
    use crate::comms::ids::{AgentId, RoomId};
    use crate::comms::model::RoomScope;

    let parent = &state.agent_id;
    let comms_session_id = session_id.to_string();

    // Derive the child agent id from the parent + the session id, validating it through
    // `AgentId::parse`. The session id alphabet (`[A-Za-z0-9._:-]`) is a subset of the agent id
    // alphabet, so the derived id is valid by construction; fall back to a sanitized id if the
    // parent contributes an out-of-alphabet byte.
    let child_candidate = format!("{parent}-{comms_session_id}");
    let child_agent = match AgentId::parse(child_candidate.clone()) {
        Ok(id) => id.into_string(),
        Err(error) => {
            // The primary id is rejected only when the parent contributes an out-of-alphabet byte.
            // Validate the fallback through the SAME parser so we never hand an invalid id to the
            // broker — a fallback that itself fails to parse is a hard error, not a silent pass.
            let fallback = format!("shell-{comms_session_id}");
            let fallback_id = AgentId::parse(fallback.clone()).map_err(|fallback_err| {
                comms_err(format!(
                    "derive child agent id: candidate {child_candidate:?} rejected ({error}) and \
                     fallback {fallback:?} also rejected ({fallback_err})"
                ))
            })?;
            tracing::warn!(
                error = %error,
                rejected_candidate_len = child_candidate.len(),
                fallback = %fallback,
                "shell_spawn: derived child agent id rejected by AgentId::parse; using fallback"
            );
            fallback_id.into_string()
        }
    };

    // The room id reuses the comms session id (valid `RoomId` by construction).
    let room = RoomId::parse(comms_session_id.clone())
        .map_err(|e| comms_err(format!("derive session room id {comms_session_id:?}: {e}")))?;
    let title = format!("shell session {comms_session_id} ({parent} -> {child_agent})");

    {
        let mut guard = comms_client(state).await?;
        let client = client_mut(&mut guard)?;
        client
            .create_room(
                room.clone(),
                RoomScope::Session(comms_session_id.clone()),
                Some(title),
            )
            .await
            .map_err(comms_err)?;
        // Subscribe the PARENT (this server) so it receives the child's posts.
        client.join_room(room.clone()).await.map_err(comms_err)?;
    }

    // The server's identity values are authoritative. Drop any caller-supplied entries for these
    // exact keys FIRST so we do not rely on last-wins env semantics — then inject the child's
    // identity + session lineage so its basemind auto-identifies and auto-joins the same session
    // room on its first `Hello`. The child reaches the same per-user broker by default, so no
    // socket env is needed.
    const IDENTITY_KEYS: [&str; 3] = [
        "BASEMIND_AGENT_ID",
        "BASEMIND_PARENT_AGENT_ID",
        "BASEMIND_SESSION_ID",
    ];
    environment.retain(|entry| {
        let key = entry.split('=').next().unwrap_or(entry);
        !IDENTITY_KEYS.contains(&key)
    });
    environment.push(format!("BASEMIND_AGENT_ID={child_agent}"));
    environment.push(format!("BASEMIND_PARENT_AGENT_ID={parent}"));
    environment.push(format!("BASEMIND_SESSION_ID={comms_session_id}"));

    Ok((room.into_string(), child_agent))
}

/// Roll back the parent's subscription to an orphaned session room after the spawn failed.
///
/// The room is created + joined before the spawn; if the spawn errors, no child will ever join, so
/// the parent's standing subscription would leak. Best-effort: a failure to leave is logged at WARN
/// (naming the orphan room id) and swallowed so the original spawn error is what propagates. There
/// is no broker `delete_room`, so the room record itself lingers until the broker is restarted —
/// only the parent's membership is reclaimed here.
#[cfg(all(feature = "comms", unix))]
async fn rollback_session_room(state: &ServerState, room_id: &str) {
    use super::helpers_comms::{client_mut, comms_client};
    use crate::comms::ids::RoomId;

    let Ok(room) = RoomId::parse(room_id.to_string()) else {
        tracing::warn!(room_id = %room_id, "shell_spawn rollback: orphan room id is unparsable");
        return;
    };
    let leave = async {
        let mut guard = comms_client(state).await?;
        let client = client_mut(&mut guard)?;
        client
            .leave_room(room)
            .await
            .map_err(super::helpers_comms::comms_err)
    };
    if let Err(error) = leave.await {
        tracing::warn!(
            error = %error,
            room_id = %room_id,
            "shell_spawn rollback: failed to leave orphaned session room; it may leak"
        );
    }
}

/// `shell_send`: write text (optionally with a newline) to a session's stdin.
pub(super) async fn run_shell_send(
    state: &ServerState,
    params: ShellSendParams,
) -> Result<CallToolResult, McpError> {
    let (id, name) = require_session(state, &params.session_id).await?;
    let session = state
        .shell_runtime
        .rmux()
        .await
        .map_err(|e| mcp_internal("connect embedded shell daemon", e))?
        .session(name)
        .await
        .map_err(|e| mcp_internal("open shell session", e))?;
    crate::shells::session::send_text(&session, &params.text, params.enter)
        .await
        .map_err(|e| mcp_internal("send to shell session", e))?;
    json_result(&serde_json::json!({ "session_id": id.to_string(), "sent": true }))
}

/// `shell_capture`: return the visible screen text of a session's primary pane.
pub(super) async fn run_shell_capture(
    state: &ServerState,
    params: ShellCaptureParams,
) -> Result<CallToolResult, McpError> {
    let (_id, name) = require_session(state, &params.session_id).await?;
    let session = state
        .shell_runtime
        .rmux()
        .await
        .map_err(|e| mcp_internal("connect embedded shell daemon", e))?
        .session(name)
        .await
        .map_err(|e| mcp_internal("open shell session", e))?;
    let text = crate::shells::session::capture(&session, params.lines)
        .await
        .map_err(|e| mcp_internal("capture shell output", e))?;
    json_result(&ShellCaptureResponse { text })
}

/// `shell_kill`: terminate a session and forget its mapping.
pub(super) async fn run_shell_kill(
    state: &ServerState,
    params: ShellKillParams,
) -> Result<CallToolResult, McpError> {
    let (id, name) = require_session(state, &params.session_id).await?;
    let session = state
        .shell_runtime
        .rmux()
        .await
        .map_err(|e| mcp_internal("connect embedded shell daemon", e))?
        .session(name)
        .await
        .map_err(|e| mcp_internal("open shell session", e))?;
    let killed = crate::shells::session::kill_session(&session)
        .await
        .map_err(|e| mcp_internal("kill shell session", e))?;
    state.shell_runtime.forget(&id).await;

    // Best-effort: drop the broker lineage row so the `sessions` keyspace does not accumulate dead
    // rows. A comms failure must not fail the kill — the session is already gone.
    #[cfg(all(feature = "comms", unix))]
    delete_session_lineage(state, id.as_str()).await;

    json_result(&ShellKillResponse {
        session_id: id.to_string(),
        killed,
    })
}

/// Best-effort removal of a killed session's broker lineage row. Failures are logged at WARN and
/// swallowed — the session is already dead, so a leftover lineage row is cosmetic, not a kill error.
#[cfg(all(feature = "comms", unix))]
async fn delete_session_lineage(state: &ServerState, session_id: &str) {
    use super::helpers_comms::{client_mut, comms_client};

    let result = async {
        let mut guard = comms_client(state).await?;
        let client = client_mut(&mut guard)?;
        client
            .delete_session(session_id)
            .await
            .map_err(super::helpers_comms::comms_err)
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(
            error = %error,
            session_id = %session_id,
            "shell_kill: failed to delete session lineage row; it may linger until broker restart"
        );
    }
}

/// `shell_broadcast`: send the same input to many sessions' primary panes.
pub(super) async fn run_shell_broadcast(
    state: &ServerState,
    params: ShellBroadcastParams,
) -> Result<CallToolResult, McpError> {
    if params.session_ids.is_empty() {
        return Err(McpError::invalid_params(
            "session_ids must not be empty",
            None,
        ));
    }
    // Validate every id up front so an unknown id fails before any delivery.
    let mut ids = Vec::with_capacity(params.session_ids.len());
    for raw in &params.session_ids {
        let (id, _name) = require_session(state, raw).await?;
        ids.push(id);
    }
    let delivered = state
        .shell_runtime
        .broadcast(&ids, &params.text, params.enter)
        .await
        .map_err(|e| mcp_internal("broadcast to shell sessions", e))?;
    json_result(&ShellBroadcastResponse { delivered })
}

/// `shell_list`: enumerate sessions across the full comms lineage, flagged by this server's
/// liveness.
///
/// Two sources are merged by `session_id`:
/// - `ShellRuntime::list()` — always present; contributes the rmux `name` + `alive` flag for the
///   sessions THIS server spawned (the only ones it holds a live rmux handle for).
/// - The shared comms broker's session lineage — present only when comms is built. It is the
///   source of truth for the parent -> child chain, so a top-level server sees grandchildren
///   spawned deeper in the chain (sessions it did not spawn directly are reported with
///   `alive = false`, since this server has no rmux handle for them).
///
/// The comms enrichment is best-effort: if the client is unavailable or the call fails, the
/// runtime-only list is returned rather than failing `shell_list`.
pub(super) async fn run_shell_list(
    state: &ServerState,
    _params: ShellListParams,
) -> Result<CallToolResult, McpError> {
    let runtime = state
        .shell_runtime
        .list()
        .await
        .map_err(|e| mcp_internal("list shell sessions", e))?;

    // Seed the merge map from this server's own sessions (name + liveness; lineage unknown here).
    // Bound as a non-`mut` `let` so the headless `shells`-only build needs no `unused_mut`
    // suppression (the project bans `allow`-style attributes); the comms-on path rebinds it `mut`.
    let by_id: ahash::AHashMap<String, ShellSessionView> = runtime
        .into_iter()
        .map(|info| {
            let session_id = info.session_id.to_string();
            (
                session_id.clone(),
                ShellSessionView {
                    session_id,
                    name: info.name.as_str().to_string(),
                    alive: info.alive,
                    parent_agent: None,
                    child_agent: None,
                    room_id: None,
                },
            )
        })
        .collect();

    // Enrich with the broker's lineage when comms is built. Best-effort: a comms failure leaves
    // `by_id` as the runtime-only view rather than failing the whole tool. The `mut` rebind lives
    // under the same gate as the enrichment, so it is absent (and harmless) in the headless build.
    #[cfg(all(feature = "comms", unix))]
    let by_id = {
        let mut by_id = by_id;
        enrich_with_lineage(state, &mut by_id).await;
        by_id
    };

    let mut sessions: Vec<ShellSessionView> = by_id.into_values().collect();
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    json_result(&ShellListResponse { sessions })
}

/// Fold the shared comms broker's session lineage into `by_id`, keyed by `session_id`.
///
/// For each lineage row: if the session is already present (this server spawned it) keep its
/// runtime `name` / `alive` and just attach the lineage fields; otherwise insert a new view with
/// `name = session_id` and `alive = false` (this server holds no rmux handle for it).
///
/// Best-effort: acquiring the client or the `list_sessions` call failing is logged at WARN and
/// swallowed, so `shell_list` still returns the runtime-only view when comms is down.
#[cfg(all(feature = "comms", unix))]
async fn enrich_with_lineage(
    state: &ServerState,
    by_id: &mut ahash::AHashMap<String, ShellSessionView>,
) {
    use super::helpers_comms::{client_mut, comms_client};

    let lineage = async {
        let mut guard = comms_client(state).await?;
        let client = client_mut(&mut guard)?;
        client
            .list_sessions()
            .await
            .map_err(super::helpers_comms::comms_err)
    }
    .await;

    let lineage = match lineage {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "shell_list: comms lineage unavailable; returning this server's own sessions only"
            );
            return;
        }
    };

    for row in lineage {
        let parent_agent = row.parent_agent.map(|agent| agent.into_string());
        let child_agent = row.child_agent.into_string();
        let room_id = row.room_id.into_string();
        // Get-or-default first, then assign the lineage fields by MOVE. A present entry (this server
        // spawned it) keeps its runtime `name` / `alive`; an absent one is seeded as a lineage-only
        // row (`name = session_id`, `alive = false`) because this process holds no rmux handle for a
        // session spawned elsewhere in the chain. Either way the three lineage values are moved in,
        // not cloned, since each is consumed exactly once.
        let view = by_id
            .entry(row.session_id.clone())
            .or_insert_with(|| ShellSessionView {
                name: row.session_id.clone(),
                session_id: row.session_id,
                alive: false,
                parent_agent: None,
                child_agent: None,
                room_id: None,
            });
        view.parent_agent = parent_agent;
        view.child_agent = Some(child_agent);
        view.room_id = Some(room_id);
    }
}
