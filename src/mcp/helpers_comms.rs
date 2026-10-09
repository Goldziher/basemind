//! The `agents` domain dispatcher and the helper bodies for its eighteen modes.
//!
//! [`run_agents`] is the entry the `#[tool]` shim calls: it validates the flat [`AgentsParams`]
//! against the selected [`AgentsMode`] and delegates to the per-mode body. Each `run_<mode>` is a
//! thin proxy: acquire the lazily-connected [`CommsClient`](crate::comms::client::CommsClient) from
//! [`ServerState`], inject the server's resolved scope context (and identity, already baked into the
//! connected client), call the matching client method, and `json_result` the front-matter response.
//! Modes `history` and `inbox` surface front-matter ONLY — bodies are fetched exclusively through
//! mode `message`.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tokio::sync::Mutex;

use super::ServerState;
use super::helpers::json_result;
use super::mode::{AgentsMode, reject_unsupported};
use super::types_comms::{
    AgentListParams, AgentListResponse, AgentRegisterParams, AgentRegisterResponse, AgentSummary, AgentsParams,
    CursorAdvance, InboxAckParams, InboxAckResponse, InboxReadParams, InboxReadResponse, InboxWaitParams,
    InboxWaitResponse, MessageFrontMatter, MessageGetParams, MessageGetResponse, ThreadArchiveParams,
    ThreadArchiveResponse, ThreadHistoryParams, ThreadHistoryResponse, ThreadJoinParams, ThreadLeaveParams,
    ThreadListParams, ThreadListResponse, ThreadMemberChangeResponse, ThreadMemberParams, ThreadMembersParams,
    ThreadMembersResponse, ThreadMembershipResponse, ThreadPostParams, ThreadPostResponse, ThreadStartParams,
    ThreadStartResponse, ThreadSummary,
};
use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::ids::AgentId;
use crate::comms::model::now_micros;

/// Default page size when a mode omits `limit`. Mirrors the broker's `DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 100;
/// Rows the delivery probe asks for. The broker bounds its own scan to a constant past this, so the
/// probe costs the same regardless of how large the unread backlog is.
const DELIVERY_SCAN_LIMIT: u32 = 200;
/// Minimum spacing between delivery probes for one session.
const DELIVERY_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const DELIVERY_NOTICE_LIMIT: usize = 5;
/// Upper bound on dialing (and, if need be, spawning) the broker for a cached identity.
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);
const DELIVERY_CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
const DELIVERY_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// Default recency window for modes `history` / `inbox` when the caller omits `since_hours`.
const DEFAULT_SINCE_HOURS: u32 = 24;

/// Default long-poll timeout for mode `wait` when the caller omits `timeout_secs`.
const DEFAULT_WAIT_SECS: u32 = 30;

/// Hard cap on mode `wait`'s `timeout_secs`. Kept under typical MCP host request timeouts (~60 s) so
/// a wait ends with a clean `timed_out` answer rather than being abandoned by the host, and short
/// enough that one outstanding wait cannot meaningfully delay a drain.
const MAX_WAIT_SECS: u32 = 40;

/// Microseconds in one hour — the scale factor for the `since_hours` → `since_micros` cutoff.
const MICROS_PER_HOUR: i64 = 3_600_000_000;

/// Translate a caller-supplied `since_hours` window into the absolute `since_micros` cutoff. `None`
/// ⇒ the [`DEFAULT_SINCE_HOURS`] default; `Some(0)` ⇒ `None` (all history); otherwise `now - hours`.
fn since_cutoff(since_hours: Option<u32>) -> Option<i64> {
    let hours = since_hours.unwrap_or(DEFAULT_SINCE_HOURS);
    if hours == 0 {
        None
    } else {
        Some(now_micros() - i64::from(hours) * MICROS_PER_HOUR)
    }
}

/// Map a [`CommsClientError`](crate::comms::client::CommsClientError) into an MCP error with a
/// stable `comms:` prefix so agents can route on it.
pub(super) fn comms_err(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("comms: {error}"), None)
}

/// Connect this MCP state to the comms broker without probing the daemon from a request it hosts.
async fn connect_comms_client(state: &ServerState, target: AgentId) -> Result<CommsClient, McpError> {
    let (remote, cwd) = scope_context_for(&state.shared.root);
    if state.shared.host.is_some() {
        let paths = crate::comms::singleton::resolve_paths().map_err(comms_err)?;
        return CommsClient::connect(&paths, target, remote, cwd)
            .await
            .map_err(comms_err);
    }
    CommsClient::ensure_and_connect(target, remote, cwd)
        .await
        .map_err(comms_err)
}

/// Validate the ≥2-of-3 addressing rule for mode `thread_start` client-side, so the caller gets a clear
/// error without a broker round-trip. The broker enforces the SAME rule; this is a fast pre-check.
/// The caller (creator) is always an implicit member, so `members` counts only when it names at
/// least one agent OTHER than the caller.
pub(super) fn validate_thread_dimensions(
    subject: Option<&str>,
    path: Option<&str>,
    members: &[AgentId],
    creator: &AgentId,
) -> Result<(), McpError> {
    let has_subject = subject.is_some_and(|s| !s.is_empty());
    let has_path = path.is_some_and(|p| !p.is_empty());
    let has_members = members.iter().any(|m| m != creator);
    let count = [has_subject, has_path, has_members].iter().filter(|b| **b).count();
    if count >= 2 {
        Ok(())
    } else {
        Err(comms_err(
            "`agents` mode=\"thread_start\" requires at least 2 of `subject` / `path` / `members` (a \
             member other than yourself); supply at least two",
        ))
    }
}

/// Resolve (lazily connecting + caching) the comms-broker client for the requested identity.
///
/// `as_agent` selects a sub-identity to act as; `None` resolves the server's own `agent_id`.
pub(super) async fn resolve_comms_client(
    state: &ServerState,
    as_agent: Option<String>,
) -> Result<Arc<Mutex<CommsClient>>, McpError> {
    let target = match as_agent {
        Some(raw) => AgentId::parse(raw.clone()).map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    // Hold the map lock only to find or create this identity's cell. Connecting happens under the
    // cell's own once-init, so a slow or wedged connect for one identity cannot block lookups (or
    // connects) for any other, and a failed connect leaves the cell empty for the next caller.
    let cell = {
        let mut map = state.comms_clients.lock().await;
        match map.get(&target) {
            Some(cell) => cell.clone(),
            None => {
                let cell = Arc::new(tokio::sync::OnceCell::new());
                // ~keep `put` past the cap drops the least-recently-used cell; callers hold the
                // ~keep returned `Arc`, so an evicted client stays alive until its last in-flight
                // ~keep handle drops, and the identity reconnects on next use.
                map.put(target.clone(), cell.clone());
                cell
            }
        }
    };
    let handle = cell
        .get_or_try_init(|| async {
            let client = tokio::time::timeout(CONNECT_BUDGET, connect_comms_client(state, target.clone()))
                .await
                .map_err(|_| {
                    comms_err(format!(
                        "broker unresponsive: connect did not finish within {}s (retryable)",
                        CONNECT_BUDGET.as_secs()
                    ))
                })??;
            Ok::<_, McpError>(Arc::new(Mutex::new(client)))
        })
        .await?;
    Ok(handle.clone())
}

/// Return a bounded, exactly-once-per-session front-matter notice for an ordinary tool response.
/// Failure and contention are deliberately silent so mailbox delivery can never make another MCP
/// capability slow or unavailable.
pub(super) async fn take_delivery_notice(state: &ServerState) -> Option<String> {
    // Contention means another tool call is already probing; skip rather than queue behind it.
    let mut probe = state.delivery_probe.try_lock().ok()?;
    if probe
        .last_probe
        .is_some_and(|at| at.elapsed() < DELIVERY_PROBE_INTERVAL)
    {
        return None;
    }
    probe.last_probe = Some(std::time::Instant::now());
    if probe.client.is_none() {
        // Dialing gets its own, longer budget: a daemon that has to be reached or spawned would
        // otherwise never finish inside the read budget and the probe would never connect.
        match tokio::time::timeout(DELIVERY_CONNECT_BUDGET, connect_ephemeral_client(state)).await {
            Ok(Ok(client)) => probe.client = Some(client),
            _ => return None,
        }
    }
    let page = match tokio::time::timeout(DELIVERY_BUDGET, probe_inbox(&mut probe.client)).await {
        Ok(Ok(page)) => page,
        // A failed or timed-out probe leaves the connection in an unknown state: drop it so the next
        // probe dials a fresh one. It is private to the probe, so nothing else is disturbed.
        _ => {
            probe.client = None;
            return None;
        }
    };
    drop(probe);
    let (messages, unread, _) = page;
    let mut delivered = state.delivered_notifications.lock().await;
    let unseen: Vec<_> = messages
        .into_iter()
        .filter(|message| !delivered.contains(&message.meta.id))
        .collect();
    if unseen.is_empty() {
        return None;
    }
    let shown = unseen.len().min(DELIVERY_NOTICE_LIMIT);
    let mut notice = String::from("New basemind agent-comms message(s):\n");
    for message in unseen.iter().take(shown) {
        delivered.put(message.meta.id.clone(), ());
        notice.push_str(&format!(
            "- [{}#{}] from {}: {}\n",
            message.meta.thread.as_str(),
            message.seq,
            message.meta.from.as_str(),
            message.meta.subject
        ));
    }
    let overflow = unseen.len().saturating_sub(shown).saturating_add(unread as usize);
    if overflow > 0 {
        notice.push_str(&format!(
            "- {overflow} more unread; use agents mode inbox to review them.\n"
        ));
    }
    notice.push_str("Use agents mode message with the message id to read a body; ack only after handling it.");
    Some(notice)
}

/// One bounded inbox read over the probe's private connection.
async fn probe_inbox(
    slot: &mut Option<CommsClient>,
) -> Result<
    (
        Vec<crate::comms::protocol::SeqMeta>,
        u32,
        Option<crate::comms::cursor::Cursor>,
    ),
    McpError,
> {
    let client = slot
        .as_mut()
        .ok_or_else(|| comms_err("delivery probe has no connection"))?;
    client
        .read_inbox(None, None, None, DELIVERY_SCAN_LIMIT, false, None)
        .await
        .map_err(comms_err)
}

/// Open a fresh, un-cached broker connection for the server's own identity. Long forwarded
/// operations (rescan / embed) use this instead of [`resolve_comms_client`] so they never hold the
/// shared per-identity client mutex that interactive comms tools + `resolved_refs` reads serialize
/// on — a multi-minute scan/embed would otherwise head-of-line-block every other comms call for
/// that identity (observed: a peer agent's message_get / thread_join / inbox_read all stalled for
/// minutes behind a forwarded embed pass). The returned client is owned and dropped by the caller.
pub(super) async fn connect_ephemeral_client(state: &ServerState) -> Result<CommsClient, McpError> {
    let target = AgentId::parse(state.agent_id.clone())
        .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?;
    connect_comms_client(state, target).await
}

/// Route a CORE memory op to the machine's sole fjall writer and return the outcome.
///
/// On the daemon-HOSTED path ([`ServerState`]'s shared `host` is `Some`) the writer pool is
/// in-process, so the op runs directly against it on a blocking thread — no socket loopback, which on
/// the daemon would be the daemon dialing itself. Every other `daemon_writer` serve has no host and
/// forwards over the socket. Both run the identical `run_memory_op` writer-side, so callers see one
/// outcome shape. Centralizing the host-vs-forward choice here keeps each call site's op-construction
/// and outcome-match unchanged while the transport branch lives in exactly one place.
#[cfg(feature = "memory")]
pub(super) async fn dispatch_memory_op(
    state: &ServerState,
    op: crate::comms::memory_proto::MemoryOp,
) -> Result<crate::comms::memory_proto::MemoryOutcome, McpError> {
    if let Some(host) = &state.shared.host {
        let host = Arc::clone(host);
        let root = state.shared.root.clone();
        let scope = state.shared.scope.clone();
        return tokio::task::spawn_blocking(move || host.host_memory(&root, &scope, op))
            .await
            .map_err(|error| McpError::internal_error(format!("host memory task panicked: {error}"), None))?
            .map_err(|error| McpError::internal_error(format!("host memory: {error}"), None));
    }
    let client = resolve_comms_client(state, None).await?;
    let mut guard = client.lock().await;
    guard
        .memory_op(state.shared.root.clone(), state.shared.scope.clone(), op)
        .await
        .map_err(comms_err)
}

/// Route a PROPOSAL governance op to the machine's sole fjall writer and return the outcome. Same
/// host-vs-forward dispatch as [`dispatch_memory_op`], running `run_governance_op` writer-side.
#[cfg(feature = "memory")]
pub(super) async fn dispatch_governance_op(
    state: &ServerState,
    op: crate::comms::proposals_proto::GovernanceOp,
) -> Result<crate::comms::proposals_proto::GovernanceOutcome, McpError> {
    if let Some(host) = &state.shared.host {
        let host = Arc::clone(host);
        let root = state.shared.root.clone();
        let scope = state.shared.scope.clone();
        return tokio::task::spawn_blocking(move || host.host_governance(&root, &scope, op))
            .await
            .map_err(|error| McpError::internal_error(format!("host governance task panicked: {error}"), None))?
            .map_err(|error| McpError::internal_error(format!("host governance: {error}"), None));
    }
    let client = resolve_comms_client(state, None).await?;
    let mut guard = client.lock().await;
    guard
        .governance_op(state.shared.root.clone(), state.shared.scope.clone(), op)
        .await
        .map_err(comms_err)
}

/// Clamp a caller-supplied limit to `[1, MAX_LIMIT]`, defaulting when absent.
fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, crate::comms::daemon::MAX_LIMIT)
}

/// Fail a mode that was given a field belonging to some other mode.
///
/// Inverted against `allowed` rather than listing every rejected field per mode: with sixteen modes
/// and twenty-three sibling fields, an explicit per-mode reject list is where a newly added field
/// silently becomes accept-everywhere.
fn reject_foreign_fields(mode: AgentsMode, present: &[(&str, bool)], allowed: &[&str]) -> Result<(), McpError> {
    let foreign: Vec<(&str, bool)> = present
        .iter()
        .filter(|(field, _)| !allowed.contains(field))
        .copied()
        .collect();
    reject_unsupported(AgentsMode::DOMAIN, mode.as_str(), &foreign)
}

/// Unwrap a field this mode cannot run without, naming the exact `mode`/field pair.
fn require_field<T>(mode: AgentsMode, field: &str, value: Option<T>) -> Result<T, McpError> {
    value.ok_or_else(|| {
        McpError::invalid_params(
            format!("`{}` mode=\"{}\" requires `{field}`", AgentsMode::DOMAIN, mode.as_str()),
            None,
        )
    })
}

/// Dispatch the single `agents` tool onto the per-mode body its `mode` selects.
///
/// Validation runs before the broker connection so a malformed call costs no daemon round-trip, and
/// fields belonging to another mode are rejected rather than dropped: a silently ignored `thread` on
/// an `inbox` call reads to an agent as a successful single-thread read.
pub(super) async fn run_agents(
    state: &ServerState,
    params: AgentsParams,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<CallToolResult, McpError> {
    let AgentsParams {
        mode,
        thread,
        member,
        members,
        message_id,
        message_ids,
        to_seq,
        subject,
        subject_contains,
        path,
        body,
        tags,
        reply_to,
        include_archived,
        mark_read,
        cursor,
        limit,
        since_hours,
        timeout_secs,
        name,
        description,
        version,
        skills,
        apply,
        message_ttl_hours,
        thread_idle_hours,
        thread_retention_hours,
        agent_ttl_hours,
        claim_ttl_hours,
        as_agent,
    } = params;
    // `as_agent` is deliberately absent: it selects the identity the call runs as, not what the call
    // does, so every mode accepts it and no allow-list needs to repeat it. ~keep
    let present = [
        ("thread", thread.is_some()),
        ("member", member.is_some()),
        ("members", members.is_some()),
        ("message_id", message_id.is_some()),
        ("message_ids", message_ids.is_some()),
        ("to_seq", to_seq.is_some()),
        ("subject", subject.is_some()),
        ("subject_contains", subject_contains.is_some()),
        ("path", path.is_some()),
        ("body", body.is_some()),
        ("tags", tags.is_some()),
        ("reply_to", reply_to.is_some()),
        ("include_archived", include_archived.is_some()),
        ("mark_read", mark_read.is_some()),
        ("cursor", cursor.is_some()),
        ("limit", limit.is_some()),
        ("since_hours", since_hours.is_some()),
        ("timeout_secs", timeout_secs.is_some()),
        ("name", name.is_some()),
        ("description", description.is_some()),
        ("version", version.is_some()),
        ("skills", skills.is_some()),
        ("apply", apply.is_some()),
        ("message_ttl_hours", message_ttl_hours.is_some()),
        ("thread_idle_hours", thread_idle_hours.is_some()),
        ("thread_retention_hours", thread_retention_hours.is_some()),
        ("agent_ttl_hours", agent_ttl_hours.is_some()),
        ("claim_ttl_hours", claim_ttl_hours.is_some()),
    ];
    reject_foreign_fields(mode, &present, allowed_fields(mode))?;
    validate_cleanup_apply(mode, apply)?;

    match mode {
        AgentsMode::Register => {
            run_agent_register(
                state,
                AgentRegisterParams {
                    name: name.unwrap_or_default(),
                    description: description.unwrap_or_default(),
                    version: version.unwrap_or_default(),
                    skills: skills.unwrap_or_default(),
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::List => run_agent_list(state, AgentListParams { thread, as_agent }).await,
        AgentsMode::ThreadStart => {
            run_thread_start(
                state,
                ThreadStartParams {
                    subject,
                    path,
                    members: members.unwrap_or_default(),
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::ThreadList => {
            run_thread_list(
                state,
                ThreadListParams {
                    subject_contains,
                    include_archived: include_archived.unwrap_or(false),
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Join => {
            run_thread_join(
                state,
                ThreadJoinParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Leave => {
            run_thread_leave(
                state,
                ThreadLeaveParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Members => {
            run_thread_members(
                state,
                ThreadMembersParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::AddMember => {
            run_thread_add_member(
                state,
                ThreadMemberParams {
                    thread: require_field(mode, "thread", thread)?,
                    member: require_field(mode, "member", member)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::RemoveMember => {
            run_thread_remove_member(
                state,
                ThreadMemberParams {
                    thread: require_field(mode, "thread", thread)?,
                    member: require_field(mode, "member", member)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Archive => {
            run_thread_archive(
                state,
                ThreadArchiveParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Post => {
            run_thread_post(
                state,
                ThreadPostParams {
                    thread: require_field(mode, "thread", thread)?,
                    subject: require_field(mode, "subject", subject)?,
                    body,
                    tags,
                    reply_to,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::History => {
            run_thread_history(
                state,
                ThreadHistoryParams {
                    thread: require_field(mode, "thread", thread)?,
                    cursor,
                    limit,
                    since_hours,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Message => {
            run_message_get(
                state,
                MessageGetParams {
                    message_id: require_field(mode, "message_id", message_id)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Inbox => {
            run_inbox_read(
                state,
                InboxReadParams {
                    cursor,
                    limit,
                    mark_read: mark_read.unwrap_or(false),
                    since_hours,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Ack => {
            run_inbox_ack(
                state,
                InboxAckParams {
                    message_ids: message_ids.unwrap_or_default(),
                    thread,
                    to_seq,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Wait => {
            run_inbox_wait(
                state,
                cancel,
                InboxWaitParams {
                    timeout_secs,
                    thread,
                    since_hours,
                    cursor,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Cleanup => {
            let handle = resolve_comms_client(state, as_agent).await?;
            let mut client = handle.lock().await;
            let report = client
                .cleanup_agents(
                    apply.unwrap_or(false),
                    hours_or_default(message_ttl_hours, crate::comms::store::MESSAGE_TTL),
                    hours_or_default(thread_idle_hours, crate::comms::store::THREAD_IDLE_TTL),
                    hours_or_default(thread_retention_hours, crate::comms::store::THREAD_RETENTION_TTL),
                    hours_or_default(agent_ttl_hours, crate::comms::store::EPHEMERAL_AGENT_TTL),
                    hours_or_default(claim_ttl_hours, crate::comms::identity::CLAIM_TTL),
                )
                .await
                .map_err(comms_err)?;
            json_result(&report)
        }
        AgentsMode::Status => {
            let handle = resolve_comms_client(state, as_agent).await?;
            let mut client = handle.lock().await;
            let report = client
                .agents_status(hours_or_default(
                    agent_ttl_hours,
                    crate::comms::store::EPHEMERAL_AGENT_TTL,
                ))
                .await
                .map_err(comms_err)?;
            json_result(&report)
        }
    }
}

/// The sibling fields each mode accepts. Everything else present on the call is rejected by
/// [`reject_foreign_fields`], so a parameter an agent believed took effect never silently doesn't.
fn allowed_fields(mode: AgentsMode) -> &'static [&'static str] {
    match mode {
        AgentsMode::Register => &["name", "description", "version", "skills"],
        AgentsMode::List => &["thread"],
        AgentsMode::ThreadStart => &["subject", "path", "members"],
        AgentsMode::ThreadList => &["subject_contains", "include_archived"],
        AgentsMode::Join | AgentsMode::Leave | AgentsMode::Members | AgentsMode::Archive => &["thread"],
        AgentsMode::AddMember | AgentsMode::RemoveMember => &["thread", "member"],
        AgentsMode::Post => &["thread", "subject", "body", "tags", "reply_to"],
        AgentsMode::History => &["thread", "cursor", "limit", "since_hours"],
        AgentsMode::Message => &["message_id"],
        AgentsMode::Inbox => &["cursor", "limit", "mark_read", "since_hours"],
        AgentsMode::Ack => &["message_ids", "thread", "to_seq"],
        AgentsMode::Wait => &["thread", "timeout_secs", "since_hours", "cursor"],
        AgentsMode::Cleanup => &[
            "apply",
            "message_ttl_hours",
            "thread_idle_hours",
            "thread_retention_hours",
            "agent_ttl_hours",
            "claim_ttl_hours",
        ],
        AgentsMode::Status => &["agent_ttl_hours"],
    }
}

fn hours_or_default(hours: Option<u32>, default: std::time::Duration) -> u64 {
    hours.map_or(default.as_secs(), |value| u64::from(value).saturating_mul(60 * 60))
}

fn validate_cleanup_apply(mode: AgentsMode, apply: Option<bool>) -> Result<(), McpError> {
    if mode == AgentsMode::Cleanup && apply == Some(true) {
        return Err(comms_err(
            "`agents` mode=\"cleanup\" is preview-only over MCP; use the CLI with `--apply`",
        ));
    }
    Ok(())
}

#[path = "helpers_comms_ops.rs"]
mod ops;
use ops::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_name_the_mode_and_field_when_a_required_sibling_is_missing() {
        let error = require_field(AgentsMode::Post, "subject", None::<String>).expect_err("a bodyless post fails");
        assert_eq!(error.message.to_string(), "`agents` mode=\"post\" requires `subject`");
    }

    #[test]
    fn should_reject_a_post_whose_body_was_supplied_but_empty() {
        for empty in ["", "   ", "\n\n", " \t \n "] {
            let error = reject_empty_body(Some(empty)).expect_err("an empty body is a caller mistake");
            assert_eq!(
                error.message.to_string(),
                "`agents` mode=\"post\" was given an empty `body`; omit `body` entirely for a subject-only post",
                "input {empty:?} must be refused with the corrective message"
            );
        }
    }

    #[test]
    fn should_accept_an_omitted_body_and_any_body_with_content() {
        reject_empty_body(None).expect("a subject-only post stays legal");
        reject_empty_body(Some("real content")).expect("a normal body posts");
        // Leading/trailing blanks are trimmed only to TEST emptiness — a body that merely starts
        // with a newline still carries content and must post unchanged.
        reject_empty_body(Some("\n# heading\n")).expect("padded content is still content");
    }

    #[test]
    fn should_reject_a_field_that_belongs_to_another_mode() {
        let error = reject_foreign_fields(
            AgentsMode::Inbox,
            &[("thread", true), ("limit", true)],
            allowed_fields(AgentsMode::Inbox),
        )
        .expect_err("`thread` is an `ack`/`wait` field, not an `inbox` one");
        let message = error.message.to_string();
        assert_eq!(message, "`agents` mode `inbox` does not accept `thread`");
    }

    #[test]
    fn should_allow_as_agent_on_every_mode_without_repeating_it_per_allow_list() {
        for mode in AgentsMode::ALL {
            assert!(
                !allowed_fields(*mode).contains(&"as_agent"),
                "`as_agent` is universal and must not be listed per mode ({mode})"
            );
        }
    }

    #[test]
    fn cleanup_rejects_apply_over_mcp_before_connecting() {
        let error = validate_cleanup_apply(AgentsMode::Cleanup, Some(true)).expect_err("apply must be CLI-only");
        assert_eq!(
            error.message.to_string(),
            "comms: `agents` mode=\"cleanup\" is preview-only over MCP; use the CLI with `--apply`"
        );
    }
}
