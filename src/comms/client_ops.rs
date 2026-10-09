//! Thread, message, and worktree RPCs of [`CommsClient`](super::CommsClient). Split out of
//! `client.rs` to keep it under the 1000-line `rust-max-lines` cap.

use super::*;

impl CommsClient {
    /// Register or update this agent's card.
    pub async fn register_agent(&mut self, card: AgentCard) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Register { card }, "register").await
    }

    /// List known agents, optionally restricted to members of one thread.
    pub async fn list_agents(&mut self, thread: Option<ThreadId>) -> Result<Vec<AgentRecord>, CommsClientError> {
        match self.request(CommsRequest::ListAgents { thread }).await? {
            CommsResponse::Agents(a) => Ok(a),
            other => Err(self.shape_err(other, "list_agents")),
        }
    }

    /// Start a thread addressed by at least two of subject / path / members. Returns the thread.
    pub async fn start_thread(
        &mut self,
        subject: Option<String>,
        path: Option<String>,
        members: Vec<AgentId>,
    ) -> Result<Thread, CommsClientError> {
        match self
            .request(CommsRequest::ThreadStart { subject, path, members })
            .await?
        {
            CommsResponse::Thread(t) => Ok(t),
            other => Err(self.shape_err(other, "start_thread")),
        }
    }

    /// List threads discoverable to this agent: member OR cwd matches the path glob OR the subject
    /// filter matches. Never all threads.
    pub async fn list_threads(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        subject_contains: Option<String>,
        include_archived: bool,
    ) -> Result<Vec<Thread>, CommsClientError> {
        match self
            .request(CommsRequest::ThreadList {
                remote,
                cwd,
                subject_contains,
                include_archived,
            })
            .await?
        {
            CommsResponse::Threads(t) => Ok(t),
            other => Err(self.shape_err(other, "list_threads")),
        }
    }

    /// Join a thread (durable membership; drives the inbox).
    pub async fn join_thread(&mut self, thread: ThreadId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadJoin { thread }, "join_thread").await
    }

    /// Leave a thread.
    pub async fn leave_thread(&mut self, thread: ThreadId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadLeave { thread }, "leave_thread")
            .await
    }

    /// List the members of a thread.
    pub async fn thread_members(&mut self, thread: ThreadId) -> Result<Vec<AgentId>, CommsClientError> {
        match self.request(CommsRequest::ThreadMembers { thread }).await? {
            CommsResponse::Members { members } => Ok(members),
            other => Err(self.shape_err(other, "thread_members")),
        }
    }

    /// Add a member to a thread (creator only).
    pub async fn add_member(&mut self, thread: ThreadId, member: AgentId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadAddMember { thread, member }, "add_member")
            .await
    }

    /// Remove a member from a thread (creator only).
    pub async fn remove_member(&mut self, thread: ThreadId, member: AgentId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadRemoveMember { thread, member }, "remove_member")
            .await
    }

    /// Archive a thread (creator only).
    pub async fn archive_thread(&mut self, thread: ThreadId) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::ThreadArchive { thread }, "archive_thread")
            .await
    }

    /// Post a message to a thread. Returns the new message id.
    pub async fn post_message(
        &mut self,
        thread: ThreadId,
        subject: String,
        body: Vec<u8>,
        tags: Vec<String>,
        reply_to: Option<String>,
    ) -> Result<String, CommsClientError> {
        match self
            .request(CommsRequest::ThreadPost {
                thread,
                subject,
                tags,
                reply_to,
                body,
            })
            .await?
        {
            CommsResponse::Posted { message_id } => Ok(message_id),
            other => Err(self.shape_err(other, "post_message")),
        }
    }

    /// Acknowledge inbox messages by advancing this agent's per-thread read cursors. Pass
    /// `message_ids` to ack specific messages (each resolved to its `(thread, seq)`), and/or a
    /// `(thread, to_seq)` pair to bulk-ack everything up to `to_seq` in that thread. Returns the
    /// count of acked ids and the `(thread, new_seq)` cursors that advanced.
    pub async fn ack_inbox(
        &mut self,
        message_ids: Vec<String>,
        thread: Option<ThreadId>,
        to_seq: Option<u64>,
    ) -> Result<(u32, Vec<(String, u64)>), CommsClientError> {
        match self
            .request(CommsRequest::AckInbox {
                message_ids,
                thread,
                to_seq,
            })
            .await?
        {
            CommsResponse::Acked {
                acked,
                cursors_advanced,
            } => Ok((acked, cursors_advanced)),
            other => Err(self.shape_err(other, "ack_inbox")),
        }
    }

    /// Read a thread's history (front-matter only), oldest-first. `since_micros` is an absolute
    /// recency cutoff; `None` returns the full log.
    pub async fn read_history(
        &mut self,
        thread: ThreadId,
        cursor: Option<Cursor>,
        limit: u32,
        since_micros: Option<i64>,
    ) -> Result<(Vec<SeqMeta>, Option<Cursor>), CommsClientError> {
        match self
            .request(CommsRequest::ThreadHistory {
                thread,
                cursor,
                limit: Some(limit),
                since_micros,
            })
            .await?
        {
            CommsResponse::History { messages, next_cursor } => Ok((messages, next_cursor)),
            other => Err(self.shape_err(other, "read_history")),
        }
    }

    /// Fetch a single message body by id. `None` when the id is unknown.
    pub async fn get_body(&mut self, message_id: String) -> Result<Option<Vec<u8>>, CommsClientError> {
        match self.request(CommsRequest::GetBody { message_id }).await? {
            CommsResponse::Body { body } => Ok(body),
            other => Err(self.shape_err(other, "get_body")),
        }
    }

    /// Read this agent's inbox across subscribed rooms. Returns the page, the count of unread
    /// remaining after the page, and the next cursor.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub async fn read_inbox(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        cursor: Option<Cursor>,
        limit: u32,
        mark_read: bool,
        since_micros: Option<i64>,
    ) -> Result<(Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        match self
            .request(CommsRequest::Inbox {
                remote,
                cwd,
                cursor,
                limit: Some(limit),
                mark_read,
                since_micros,
            })
            .await?
        {
            CommsResponse::Inbox {
                messages,
                unread,
                next_cursor,
            } => Ok((messages, unread, next_cursor)),
            other => Err(self.shape_err(other, "read_inbox")),
        }
    }

    /// Open a notification stream for a thread. Returns the subscription handle; subsequent
    /// [`CommsClient::next_notification`] calls surface posts to that thread.
    pub async fn subscribe(&mut self, thread: ThreadId) -> Result<u64, CommsClientError> {
        match self.request(CommsRequest::Subscribe { thread }).await? {
            CommsResponse::Subscribed { sub } => Ok(sub),
            other => Err(self.shape_err(other, "subscribe")),
        }
    }

    /// Open a notification stream for THIS agent's inbox: a push for every subsequent post to a
    /// thread the agent is already a member of (self-authored posts excluded), or — when `thread`
    /// is `Some` — restricted to that one thread. Passive: unlike [`CommsClient::subscribe`], it
    /// does NOT join anything. Backs [`CommsClient::wait_inbox`].
    pub async fn subscribe_inbox(&mut self, thread: Option<ThreadId>) -> Result<u64, CommsClientError> {
        match self.request(CommsRequest::SubscribeInbox { thread }).await? {
            CommsResponse::Subscribed { sub } => Ok(sub),
            other => Err(self.shape_err(other, "subscribe_inbox")),
        }
    }

    /// Cancel a notification stream opened by [`CommsClient::subscribe`] or
    /// [`CommsClient::subscribe_inbox`].
    pub async fn unsubscribe(&mut self, sub: u64) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Unsubscribe { sub }, "unsubscribe").await
    }

    /// Long-poll the inbox: subscribe FIRST, then do one immediate non-blocking inbox read, then —
    /// only if that read came back empty — block on the notification stream up to `timeout`. The
    /// subscribe-first ordering is the race fix: a post landing between the caller's last read and
    /// this call is never lost, because the sink is already live before the immediate read runs.
    /// Unsubscribes on EVERY exit path (early return, wake, shutdown, timeout, or error) so a
    /// failed/aborted wait never leaks a sink.
    ///
    /// Returns `(timed_out, rows, unread, next_cursor)`:
    /// * a non-empty immediate read, or a wake from a post, yields `timed_out = false` with a
    ///   freshly re-read, consistent page (never marks read — see `inbox_ack` / `inbox_read`).
    /// * a daemon [`CommsNotification::Shutdown`] (the link is about to die) or the socket closing
    ///   yields `timed_out = true` with an empty page — the caller reconnects and retries.
    /// * the `timeout` elapsing with nothing new yields `timed_out = true`, carrying the unread
    ///   count observed by the immediate read.
    #[allow(clippy::too_many_arguments)]
    pub async fn wait_inbox(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        thread: Option<ThreadId>,
        since_micros: Option<i64>,
        cursor: Option<Cursor>,
        limit: u32,
        timeout: std::time::Duration,
    ) -> Result<(bool, Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        self.wait_inbox_unseen(
            remote,
            cwd,
            thread,
            since_micros,
            cursor,
            limit,
            timeout,
            &CancellationToken::new(),
            |_| false,
        )
        .await
    }

    /// [`CommsClient::wait_inbox`] that skips rows the caller already `is_seen` and stops promptly
    /// when `cancel` fires.
    ///
    /// `is_seen` is what keeps a poll loop that never acks from spinning: an unread-but-already-
    /// reported backlog no longer satisfies the immediate read, so the call blocks for something
    /// genuinely new. On cancellation the sink is NOT unsubscribed over the wire (the caller is gone;
    /// the request would only queue behind a dead consumer): the caller drops this client, and the
    /// broker reaps the subscription when the link closes. Returns [`CommsClientError::Cancelled`].
    #[allow(clippy::too_many_arguments)]
    pub async fn wait_inbox_unseen(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        thread: Option<ThreadId>,
        since_micros: Option<i64>,
        cursor: Option<Cursor>,
        limit: u32,
        timeout: std::time::Duration,
        cancel: &CancellationToken,
        is_seen: impl Fn(&SeqMeta) -> bool,
    ) -> Result<(bool, Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        let sub = tokio::select! {
            () = cancel.cancelled() => return Err(CommsClientError::Cancelled),
            sub = self.subscribe_inbox(thread) => sub?,
        };
        let outcome = self
            .wait_inbox_after_subscribe(remote, cwd, since_micros, cursor, limit, timeout, cancel, is_seen)
            .await;
        if !matches!(outcome, Err(CommsClientError::Cancelled)) {
            let _ = self.unsubscribe(sub).await;
        }
        outcome
    }

    /// The body of [`CommsClient::wait_inbox_unseen`] once the sink is live: check, then block, and
    /// re-check on every wake until something unseen shows up or `timeout` elapses.
    #[allow(clippy::too_many_arguments)]
    async fn wait_inbox_after_subscribe(
        &mut self,
        remote: Option<String>,
        cwd: Option<PathBuf>,
        since_micros: Option<i64>,
        cursor: Option<Cursor>,
        limit: u32,
        timeout: std::time::Duration,
        cancel: &CancellationToken,
        is_seen: impl Fn(&SeqMeta) -> bool,
    ) -> Result<(bool, Vec<SeqMeta>, u32, Option<Cursor>), CommsClientError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (rows, unread, next) = tokio::select! {
                () = cancel.cancelled() => return Err(CommsClientError::Cancelled),
                read = self.read_inbox(remote.clone(), cwd.clone(), cursor.clone(), limit, false, since_micros) => read?,
            };
            let fresh: Vec<SeqMeta> = rows.into_iter().filter(|row| !is_seen(row)).collect();
            if !fresh.is_empty() {
                return Ok((false, fresh, unread, next));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok((true, Vec::new(), unread, None));
            }
            let note = tokio::select! {
                () = cancel.cancelled() => return Err(CommsClientError::Cancelled),
                note = tokio::time::timeout(remaining, self.poll_notification()) => note,
            };
            match note {
                Ok(Ok(Some(CommsNotification::Message(_)))) => continue,
                // Discovery metadata is consumed directly through `subscribe_inbox` plus
                // `poll_notification`. Keep the legacy message-only wait shape exhaustive without
                // misreporting a live wake as a timeout.
                Ok(Ok(Some(CommsNotification::ThreadDiscovered(_)))) => return Ok((false, Vec::new(), unread, None)),
                Ok(Ok(Some(CommsNotification::Shutdown))) | Ok(Ok(None)) => return Ok((true, Vec::new(), 0, None)),
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => return Ok((true, Vec::new(), unread, None)),
            }
        }
    }

    /// Ask the daemon for its status snapshot.
    pub async fn status(&mut self) -> Result<StatusReport, CommsClientError> {
        match self.request(CommsRequest::Status).await? {
            CommsResponse::Status(s) => Ok(s),
            other => Err(self.shape_err(other, "status")),
        }
    }

    /// Ask the daemon to drain and stop.
    pub async fn stop(&mut self) -> Result<(), CommsClientError> {
        self.expect_ok(CommsRequest::Stop, "stop").await
    }

    /// Drain the next buffered notification, if any was received while awaiting a response.
    /// Does not block on the socket — call [`CommsClient::poll_notification`] to read one
    /// directly off the wire.
    pub fn next_notification(&mut self) -> Option<CommsNotification> {
        self.pending_notifications.pop_front()
    }

    /// Await the next notification directly from the socket (after draining any buffered ones).
    pub async fn poll_notification(&mut self) -> Result<Option<CommsNotification>, CommsClientError> {
        if let Some(n) = self.pending_notifications.pop_front() {
            return Ok(Some(n));
        }
        loop {
            match self.read_frame().await? {
                Some(CommsOut::Notification(n)) => return Ok(Some(n)),
                Some(CommsOut::Response(_) | CommsOut::Reply { .. }) => continue,
                None => return Ok(None),
            }
        }
    }

    /// List the workspaces the daemon currently holds hot (drives the `basemind statusline` CLI).
    pub async fn accessed_paths(&mut self) -> Result<Vec<AccessedWorkspace>, CommsClientError> {
        match self.request(CommsRequest::AccessedPaths).await? {
            CommsResponse::Accessed { workspaces } => Ok(workspaces),
            other => Err(self.shape_err(other, "accessed_paths")),
        }
    }

    /// List every registered workspace in the daemon's machine registry (git + plain). Read-only.
    pub async fn list_workspaces(&mut self) -> Result<Vec<crate::registry::WorkspaceRecord>, CommsClientError> {
        match self.request(CommsRequest::WorkspacesList).await? {
            CommsResponse::Workspaces { workspaces } => Ok(workspaces),
            other => Err(self.shape_err(other, "list_workspaces")),
        }
    }

    /// List the worktrees of a registered repo by id. An unknown repo id returns an empty list.
    pub async fn list_worktrees(
        &mut self,
        repo_id: String,
    ) -> Result<Vec<crate::registry::WorktreeRecord>, CommsClientError> {
        match self.request(CommsRequest::WorktreesList { repo_id }).await? {
            CommsResponse::Worktrees { worktrees } => Ok(worktrees),
            other => Err(self.shape_err(other, "list_worktrees")),
        }
    }

    /// List the local branches of a registered repo by id. An unknown repo id returns an empty list.
    pub async fn list_branches(
        &mut self,
        repo_id: String,
    ) -> Result<Vec<crate::registry::BranchRecord>, CommsClientError> {
        match self.request(CommsRequest::BranchesList { repo_id }).await? {
            CommsResponse::Branches { branches } => Ok(branches),
            other => Err(self.shape_err(other, "list_branches")),
        }
    }

    /// Advisory-claim a worktree for `claimant`. Returns `true` when the claim is now held by
    /// `claimant` (freshly taken or already theirs), `false` when another claimant holds it or the
    /// worktree is unknown.
    pub async fn claim_worktree(
        &mut self,
        repo_id: String,
        name: String,
        claimant: String,
    ) -> Result<bool, CommsClientError> {
        match self
            .request(CommsRequest::WorktreeClaim {
                repo_id,
                name,
                claimant,
            })
            .await?
        {
            CommsResponse::ClaimOutcome { held } => Ok(held),
            other => Err(self.shape_err(other, "claim_worktree")),
        }
    }

    /// Release an advisory worktree claim held by `claimant`. Returns `true` when a claim by
    /// `claimant` was cleared, `false` otherwise.
    pub async fn release_worktree(
        &mut self,
        repo_id: String,
        name: String,
        claimant: String,
    ) -> Result<bool, CommsClientError> {
        match self
            .request(CommsRequest::WorktreeRelease {
                repo_id,
                name,
                claimant,
            })
            .await?
        {
            CommsResponse::ClaimOutcome { held } => Ok(held),
            other => Err(self.shape_err(other, "release_worktree")),
        }
    }
}
