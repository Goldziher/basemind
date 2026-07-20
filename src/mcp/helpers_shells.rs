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
    ShellBroadcastParams, ShellBroadcastResponse, ShellCaptureParams, ShellCaptureResponse, ShellEnv, ShellKillParams,
    ShellKillResponse, ShellListParams, ShellListResponse, ShellSendParams, ShellSessionView, ShellSpawnParams,
    ShellSpawnResponse,
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
        return Err(McpError::invalid_params("session_id must not be empty", None));
    }
    Ok(SessionId::new(raw))
}

/// Resolve a `session_id` to its rmux session name, erroring when unknown.
async fn require_session(state: &ServerState, raw: &str) -> Result<(SessionId, rmux_sdk::SessionName), McpError> {
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
/// to a comms THREAD so the parent (this server) and the spawned child can talk bidirectionally: a
/// thread with explicit members `[parent, child]` (plus a subject, satisfying the ≥2-of-3
/// addressing rule) is started BEFORE the shell starts, and the child inherits `BASEMIND_THREAD_ID`
/// / `BASEMIND_PARENT_AGENT_ID` / `BASEMIND_AGENT_ID` in its environment. Because the child is an
/// explicit member, the thread already surfaces in its inbox — no auto-join. The coupling is
/// created atomically before the spawn: a failure aborts the spawn, so no thread-less session
/// leaks. When comms is disabled the tool behaves headless and `room_id` / `child_agent` are `None`.
pub(super) async fn run_shell_spawn(state: &ServerState, params: ShellSpawnParams) -> Result<CallToolResult, McpError> {
    if !state.config.shells.enabled {
        return Err(McpError::invalid_params(
            "shells are disabled in config ([shells].enabled = false)",
            None,
        ));
    }

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

    let session_id = state.shell_runtime.mint_session_id();

    #[cfg_attr(not(all(feature = "comms", any(unix, windows))), allow(unused_mut))]
    let mut environment = build_environment(params.env.unwrap_or_default())?;

    #[cfg(all(feature = "comms", any(unix, windows)))]
    let (room_id, child_agent) = couple_session_room(state, session_id.as_str(), &mut environment).await?;
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
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
            #[cfg(all(feature = "comms", any(unix, windows)))]
            if let Some(room) = room_id.as_deref() {
                rollback_session_room(state, room).await;
            }
            return Err(mcp_internal("spawn shell session", error));
        }
    };

    let target = crate::shells::launcher::AttachTarget {
        session_name: name.as_str().to_string(),
        socket_path: state.shell_runtime.socket_path().to_path_buf(),
        cols: state.config.shells.default_cols,
        rows: state.config.shells.default_rows,
        exe: std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("basemind")),
    };
    let attach_command = target.attach_command();

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
/// the pre-minted `session_id`, starts a THREAD with members `[parent, child]` + a subject, and
/// injects the child's identity env (plus `BASEMIND_THREAD_ID`) into `environment` BEFORE the shell
/// is spawned. Returns `(thread_id, child_agent)` on success.
///
/// The coupling is OPTIONAL: comms is an add-on, so a broker that is unreachable / down must not
/// fail the shell spawn. A comms failure is logged and the function returns `(None, None)`, leaving
/// `environment` untouched so the session spawns headless.
///
/// # Threat model
/// The child's identity is asserted purely through inherited `BASEMIND_*` env vars. A spawned child
/// is free to overwrite `BASEMIND_AGENT_ID` and claim another agent's id — the broker does not
/// cross-check. Acceptable for a local single-user dev tool (every process runs as the same uid).
#[cfg(all(feature = "comms", any(unix, windows)))]
async fn couple_session_room(
    state: &ServerState,
    session_id: &str,
    environment: &mut Vec<String>,
) -> Result<(Option<String>, Option<String>), McpError> {
    match try_couple_session_thread(state, session_id, environment).await {
        Ok((thread_id, child_agent)) => Ok((Some(thread_id), Some(child_agent))),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "shell_spawn: comms coupling unavailable; spawning the session headless"
            );
            Ok((None, None))
        }
    }
}

/// The fallible inner body of [`couple_session_room`]. On `Ok`, the thread exists (with the parent
/// and child as members), and the child's identity env + `BASEMIND_THREAD_ID` have been appended.
#[cfg(all(feature = "comms", any(unix, windows)))]
async fn try_couple_session_thread(
    state: &ServerState,
    session_id: &str,
    environment: &mut Vec<String>,
) -> Result<(String, String), McpError> {
    use super::helpers_comms::{comms_err, resolve_comms_client};
    use crate::comms::ids::AgentId;

    let parent = &state.agent_id;
    let comms_session_id = session_id.to_string();

    let child_candidate = format!("{parent}-{comms_session_id}");
    let child_agent = match AgentId::parse(child_candidate.clone()) {
        Ok(id) => id,
        Err(error) => {
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
            fallback_id
        }
    };

    let subject = format!("shell session {comms_session_id} ({parent} -> {child_agent})");
    let thread_id = {
        let handle = resolve_comms_client(state, None).await?;
        let mut client = handle.lock().await;
        let thread = client
            .start_thread(Some(subject), None, vec![child_agent.clone()])
            .await
            .map_err(comms_err)?;
        thread.id.into_string()
    };

    const IDENTITY_KEYS: [&str; 3] = ["BASEMIND_AGENT_ID", "BASEMIND_PARENT_AGENT_ID", "BASEMIND_THREAD_ID"];
    environment.retain(|entry| {
        let key = entry.split('=').next().unwrap_or(entry);
        !IDENTITY_KEYS.contains(&key)
    });
    environment.push(format!("BASEMIND_AGENT_ID={child_agent}"));
    environment.push(format!("BASEMIND_PARENT_AGENT_ID={parent}"));
    environment.push(format!("BASEMIND_THREAD_ID={thread_id}"));

    Ok((thread_id, child_agent.into_string()))
}

/// Roll back the orphaned coupling thread after the spawn failed: the parent (its creator) archives
/// it so it does not linger as an active, member-less-child thread. Best-effort — a failure is
/// logged at WARN (naming the orphan thread id) and swallowed so the original spawn error propagates.
#[cfg(all(feature = "comms", any(unix, windows)))]
async fn rollback_session_room(state: &ServerState, thread_id: &str) {
    use super::helpers_comms::resolve_comms_client;
    use crate::comms::ids::ThreadId;

    let Ok(thread) = ThreadId::parse(thread_id.to_string()) else {
        tracing::warn!(thread_id = %thread_id, "shell_spawn rollback: orphan thread id is unparsable");
        return;
    };
    let archive = async {
        let handle = resolve_comms_client(state, None).await?;
        let mut client = handle.lock().await;
        client
            .archive_thread(thread)
            .await
            .map_err(super::helpers_comms::comms_err)
    };
    if let Err(error) = archive.await {
        tracing::warn!(
            error = %error,
            thread_id = %thread_id,
            "shell_spawn rollback: failed to archive orphaned coupling thread; it may leak"
        );
    }
}

/// `shell_send`: write text (optionally with a newline) to a session's stdin.
pub(super) async fn run_shell_send(state: &ServerState, params: ShellSendParams) -> Result<CallToolResult, McpError> {
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
pub(super) async fn run_shell_kill(state: &ServerState, params: ShellKillParams) -> Result<CallToolResult, McpError> {
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

    json_result(&ShellKillResponse {
        session_id: id.to_string(),
        killed,
    })
}

/// `shell_broadcast`: send the same input to many sessions' primary panes.
pub(super) async fn run_shell_broadcast(
    state: &ServerState,
    params: ShellBroadcastParams,
) -> Result<CallToolResult, McpError> {
    if params.session_ids.is_empty() {
        return Err(McpError::invalid_params("session_ids must not be empty", None));
    }
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

/// `shell_list`: enumerate the sessions THIS server spawned (the only ones it holds a live rmux
/// handle for), each flagged by its liveness.
///
/// The thread-model comms broker keeps no session-lineage keyspace, so there is no cross-server
/// grandchild view to fold in — the list is the runtime's own sessions. The `parent_agent` /
/// `child_agent` / `room_id` fields on each row stay `None` here; a spawned session's coupling
/// thread is surfaced by `shell_spawn`'s own response instead.
pub(super) async fn run_shell_list(state: &ServerState, _params: ShellListParams) -> Result<CallToolResult, McpError> {
    let runtime = state
        .shell_runtime
        .list()
        .await
        .map_err(|e| mcp_internal("list shell sessions", e))?;

    let mut sessions: Vec<ShellSessionView> = runtime
        .into_iter()
        .map(|info| ShellSessionView {
            session_id: info.session_id.to_string(),
            name: info.name.as_str().to_string(),
            alive: info.alive,
            parent_agent: None,
            child_agent: None,
            room_id: None,
        })
        .collect();
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    json_result(&ShellListResponse { sessions })
}
