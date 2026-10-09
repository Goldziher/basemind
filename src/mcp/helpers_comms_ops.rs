//! Per-mode `run_<mode>` bodies for the `agents` dispatcher in [`helpers_comms`](super). Split out
//! of `helpers_comms.rs` to keep it under the 1000-line `rust-max-lines` cap.

use super::*;

pub(super) async fn run_agent_register(
    state: &ServerState,
    params: AgentRegisterParams,
) -> Result<CallToolResult, McpError> {
    let card = crate::comms::model::AgentCard {
        name: params.name,
        description: params.description,
        version: params.version,
        skills: params.skills,
    };
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let agent_id = client.agent().as_str().to_string();
    client.register_agent(card).await.map_err(comms_err)?;
    json_result(&AgentRegisterResponse {
        agent_id,
        registered: true,
    })
}

pub(super) async fn run_agent_list(state: &ServerState, params: AgentListParams) -> Result<CallToolResult, McpError> {
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let records = client.list_agents(params.thread).await.map_err(comms_err)?;
    let agents: Vec<AgentSummary> = records
        .iter()
        .map(|r| AgentSummary {
            agent_id: r.agent_id.as_str().to_string(),
            name: r.card.name.clone(),
            description: r.card.description.clone(),
            version: r.card.version.clone(),
            skills: r.card.skills.clone(),
            first_seen: r.first_seen,
            last_seen: r.last_seen,
        })
        .collect();
    json_result(&AgentListResponse {
        total: agents.len(),
        agents,
    })
}

pub(super) async fn run_thread_start(
    state: &ServerState,
    params: ThreadStartParams,
) -> Result<CallToolResult, McpError> {
    let creator = match &params.as_agent {
        Some(raw) => AgentId::parse(raw.clone()).map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    validate_thread_dimensions(
        params.subject.as_deref(),
        params.path.as_deref(),
        &params.members,
        &creator,
    )?;
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let thread = client
        .start_thread(params.subject, params.path, params.members)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadStartResponse {
        thread: ThreadSummary::from_thread(&thread, now_micros()),
    })
}

pub(super) async fn run_thread_list(state: &ServerState, params: ThreadListParams) -> Result<CallToolResult, McpError> {
    let (remote, cwd) = scope_context_for(&state.shared.root);
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let threads = client
        .list_threads(remote, cwd, params.subject_contains, params.include_archived)
        .await
        .map_err(comms_err)?;
    let now = now_micros();
    let summaries: Vec<ThreadSummary> = threads.iter().map(|t| ThreadSummary::from_thread(t, now)).collect();
    json_result(&ThreadListResponse {
        total: summaries.len(),
        threads: summaries,
    })
}

pub(super) async fn run_thread_join(state: &ServerState, params: ThreadJoinParams) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client.join_thread(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadMembershipResponse {
        thread: label,
        joined: true,
        left: false,
    })
}

pub(super) async fn run_thread_leave(
    state: &ServerState,
    params: ThreadLeaveParams,
) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client.leave_thread(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadMembershipResponse {
        thread: label,
        joined: false,
        left: true,
    })
}

pub(super) async fn run_thread_members(
    state: &ServerState,
    params: ThreadMembersParams,
) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let members = client.thread_members(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadMembersResponse {
        thread: label,
        members: members.iter().map(|m| m.as_str().to_string()).collect(),
    })
}

pub(super) async fn run_thread_add_member(
    state: &ServerState,
    params: ThreadMemberParams,
) -> Result<CallToolResult, McpError> {
    let thread = params.thread.as_str().to_string();
    let member = params.member.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client
        .add_member(params.thread, params.member)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadMemberChangeResponse {
        thread,
        member,
        added: true,
        removed: false,
    })
}

pub(super) async fn run_thread_remove_member(
    state: &ServerState,
    params: ThreadMemberParams,
) -> Result<CallToolResult, McpError> {
    let thread = params.thread.as_str().to_string();
    let member = params.member.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client
        .remove_member(params.thread, params.member)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadMemberChangeResponse {
        thread,
        member,
        added: false,
        removed: true,
    })
}

pub(super) async fn run_thread_archive(
    state: &ServerState,
    params: ThreadArchiveParams,
) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client.archive_thread(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadArchiveResponse {
        thread: label,
        archived: true,
    })
}

/// Reject a `body` that was supplied but holds nothing.
///
/// Omitting `body` is legal — that is a deliberate subject-only post. Supplying one that is empty or
/// whitespace-only is a caller mistake (an unresolved template, a variable that expanded to nothing),
/// and storing it is worse than refusing it: the front-matter records `body_len: 0`, and `message`
/// later returns `found: true, body: ""`, which reads as a failed retrieval rather than as a message
/// that never had content. Failing at the post surfaces the mistake where it can still be corrected.
pub(super) fn reject_empty_body(body: Option<&str>) -> Result<(), McpError> {
    if body.is_some_and(|body| body.trim().is_empty()) {
        return Err(McpError::invalid_params(
            format!(
                "`{}` mode=\"{}\" was given an empty `body`; omit `body` entirely for a subject-only post",
                AgentsMode::DOMAIN,
                AgentsMode::Post.as_str()
            ),
            None,
        ));
    }
    Ok(())
}

pub(super) async fn run_thread_post(state: &ServerState, params: ThreadPostParams) -> Result<CallToolResult, McpError> {
    reject_empty_body(params.body.as_deref())?;
    let body = params.body.unwrap_or_default().into_bytes();
    let tags = params.tags.unwrap_or_default();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let message_id = client
        .post_message(params.thread, params.subject, body, tags, params.reply_to)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadPostResponse { message_id })
}

pub(super) async fn run_thread_history(
    state: &ServerState,
    params: ThreadHistoryParams,
) -> Result<CallToolResult, McpError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let (metas, next_cursor) = client
        .read_history(params.thread, cursor, limit, since)
        .await
        .map_err(comms_err)?;
    let now = now_micros();
    let messages: Vec<MessageFrontMatter> = metas
        .iter()
        .map(|sm| MessageFrontMatter::from_seq_meta(sm, now))
        .collect();
    json_result(&ThreadHistoryResponse {
        total: messages.len(),
        messages,
        next_cursor,
    })
}

pub(super) async fn run_message_get(state: &ServerState, params: MessageGetParams) -> Result<CallToolResult, McpError> {
    let message_id = params.message_id.clone();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let body = client.get_body(params.message_id).await.map_err(comms_err)?;
    let found = body.is_some();
    let body = body.map(|b| String::from_utf8_lossy(&b).into_owned());
    json_result(&MessageGetResponse {
        message_id,
        found,
        body,
    })
}

pub(super) async fn run_inbox_read(state: &ServerState, params: InboxReadParams) -> Result<CallToolResult, McpError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let (remote, cwd) = scope_context_for(&state.shared.root);
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let (metas, unread, next_cursor) = client
        .read_inbox(remote, cwd, cursor, limit, params.mark_read, since)
        .await
        .map_err(comms_err)?;
    let now = now_micros();
    let messages: Vec<MessageFrontMatter> = metas
        .iter()
        .map(|sm| MessageFrontMatter::from_seq_meta(sm, now))
        .collect();
    json_result(&InboxReadResponse {
        total: messages.len(),
        unread,
        messages,
        next_cursor,
    })
}

pub(super) async fn run_inbox_ack(state: &ServerState, params: InboxAckParams) -> Result<CallToolResult, McpError> {
    let has_bulk = params.thread.is_some() && params.to_seq.is_some();
    if params.message_ids.is_empty() && !has_bulk {
        return Err(comms_err(
            "`agents` mode=\"ack\" requires `message_ids`, or a (`thread`, `to_seq`) pair",
        ));
    }
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let (acked, cursors) = client
        .ack_inbox(params.message_ids, params.thread, params.to_seq)
        .await
        .map_err(comms_err)?;
    let cursors_advanced: Vec<CursorAdvance> = cursors
        .into_iter()
        .map(|(thread, seq)| CursorAdvance { thread, seq })
        .collect();
    json_result(&InboxAckResponse {
        acked: acked as usize,
        cursors_advanced,
    })
}

/// Long-poll the inbox and return as soon as a peer posts (or on timeout).
///
/// LOAD-BEARING: this opens its OWN ephemeral [`CommsClient`], never the shared cached
/// `Arc<Mutex<CommsClient>>` behind [`resolve_comms_client`]. Locking that shared client for the
/// wait would hold its mutex for up to `timeout_secs`, head-of-line-blocking every OTHER comms
/// tool call for this identity (agent_list, thread_post, inbox_read, …) for the whole wait. A
/// fresh connection per wait avoids that at the cost of one extra link + broker sink per
/// outstanding call — an accepted trade-off (see the design brief's risk notes).
pub(super) async fn run_inbox_wait(
    state: &ServerState,
    cancel: &tokio_util::sync::CancellationToken,
    params: InboxWaitParams,
) -> Result<CallToolResult, McpError> {
    let timeout_secs = params.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, MAX_WAIT_SECS);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let (remote, cwd) = scope_context_for(&state.shared.root);

    let agent = match &params.as_agent {
        Some(raw) => AgentId::parse(raw.clone()).map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    let mut client = tokio::select! {
        () = cancel.cancelled() => return Err(comms_err("wait cancelled")),
        client = connect_comms_client(state, agent) => client?,
    };

    // Messages an earlier `wait` already returned must not satisfy this one: a poll loop that does
    // not `ack` would otherwise return instantly, forever, on its own backlog.
    let already_waited: std::collections::HashSet<String> = {
        let waited = state.waited_messages.lock().await;
        waited.iter().map(|(id, ())| id.clone()).collect()
    };
    // Dropping `client` on any exit (including cancellation) closes the link, and the broker reaps
    // the subscription with it.
    let (timed_out, metas, unread, next_cursor) = match client
        .wait_inbox_unseen(
            remote,
            cwd,
            params.thread,
            since,
            cursor,
            DEFAULT_LIMIT,
            std::time::Duration::from_secs(u64::from(timeout_secs)),
            cancel,
            |row| already_waited.contains(&row.meta.id),
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(crate::comms::client::CommsClientError::Cancelled) => return Err(comms_err("wait cancelled")),
        Err(error) => return Err(comms_err(error)),
    };
    drop(client);
    {
        let mut waited = state.waited_messages.lock().await;
        for row in &metas {
            waited.put(row.meta.id.clone(), ());
        }
    }

    let now = now_micros();
    let messages: Vec<MessageFrontMatter> = metas
        .iter()
        .map(|sm| MessageFrontMatter::from_seq_meta(sm, now))
        .collect();
    json_result(&InboxWaitResponse {
        timed_out,
        total: messages.len(),
        unread,
        messages,
        next_cursor,
    })
}
