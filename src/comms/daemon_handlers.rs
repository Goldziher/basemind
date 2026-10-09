//! Broker request handlers for the agent / thread / message / subscription protocol.
//!
//! Extracted from `daemon.rs` as a second `impl Broker` block to keep that file within the
//! per-file size budget. The workspace / rescan / git-history handlers stay in `daemon.rs`;
//! these are the coordination-surface handlers (`Hello`, thread lifecycle, posting, inbox,
//! subscription fan-out).

use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use super::cursor::Cursor;
use super::daemon::threads::{build_chain, mint_message_id, mint_thread_id, validate_dimensions};
use super::daemon::{Broker, LifecycleState, Session, SubScope, SubSink};
use super::daemon_handler_helpers::{
    INBOX_UNREAD_SCAN_SLACK, clamp_limit, decode_after, keep_since, need_hello, not_creator, not_member,
    reference_error, unknown_thread, upsert_high, validate_retention_policy,
};
use super::ids::{AgentId, ThreadId};
use super::model::{AgentCard, AgentKind, AgentRecord, Membership, MessageBody, MessageMeta, Thread, now_micros};
use super::protocol::{CommsNotification, CommsOut, CommsResponse, PROTO_VER, SeqMeta, StatusReport};
use super::scope;
use super::store::{self, CommsStore, CommsStoreError, MessageReferenceResolution};

pub(crate) const MIN_RETENTION_SECS: u64 = 60;
pub(crate) const MAX_RETENTION_SECS: u64 = 365 * 24 * 60 * 60;

/// The caller-supplied parts of a `ThreadPost`.
pub(super) struct PostFields {
    pub(super) subject: String,
    pub(super) tags: Vec<String>,
    pub(super) reply_to: Option<String>,
    pub(super) body: Vec<u8>,
    pub(super) idempotency_key: Option<String>,
}

impl Broker {
    pub(super) async fn on_agents_cleanup_request(&self, request: super::protocol::CommsRequest) -> CommsResponse {
        let super::protocol::CommsRequest::Cleanup {
            apply,
            message_ttl_secs,
            thread_idle_ttl_secs,
            thread_retention_ttl_secs,
            agent_ttl_secs,
            claim_ttl_secs,
        } = request
        else {
            return CommsResponse::Error {
                code: "invalid_cleanup_request".to_string(),
                message: "cleanup handler received another request variant".to_string(),
            };
        };
        if let Err(message) = validate_retention_policy(
            message_ttl_secs,
            thread_idle_ttl_secs,
            thread_retention_ttl_secs,
            agent_ttl_secs,
            claim_ttl_secs,
        ) {
            return CommsResponse::Error {
                code: "invalid_retention_policy".to_string(),
                message,
            };
        }
        self.on_agents_cleanup(
            apply,
            message_ttl_secs,
            thread_idle_ttl_secs,
            thread_retention_ttl_secs,
            agent_ttl_secs,
            claim_ttl_secs,
        )
        .await
    }

    /// The retention sweep scans every message/thread/agent range and rewrites claim files, so the
    /// whole pass runs on the blocking pool, not a runtime worker.
    pub(super) async fn on_agents_cleanup(
        &self,
        apply: bool,
        message_ttl_secs: u64,
        thread_idle_ttl_secs: u64,
        thread_retention_ttl_secs: u64,
        agent_ttl_secs: u64,
        claim_ttl_secs: u64,
    ) -> CommsResponse {
        // Every outcome (including the per-stage error codes) is a response, never a store error.
        let outcome = self
            .store_write(move |store| {
                let result = store.cleanup(
                    apply,
                    std::time::Duration::from_secs(message_ttl_secs),
                    std::time::Duration::from_secs(thread_idle_ttl_secs),
                    std::time::Duration::from_secs(thread_retention_ttl_secs),
                    std::time::Duration::from_secs(agent_ttl_secs),
                );
                Ok(match result {
                    Ok(mut report) => {
                        match crate::comms::identity::cleanup_expired_claims(
                            std::time::Duration::from_secs(claim_ttl_secs),
                            apply,
                        ) {
                            Ok(count) => report.stale_claims = count,
                            Err(error) => {
                                return Ok(CommsResponse::Error {
                                    code: "claim_cleanup_error".to_string(),
                                    message: error.to_string(),
                                });
                            }
                        }
                        if apply && let Err(error) = store.record_maintenance() {
                            return Ok(CommsResponse::Error {
                                code: "maintenance_timestamp_error".to_string(),
                                message: error.to_string(),
                            });
                        }
                        CommsResponse::Cleanup(report)
                    }
                    Err(error) => CommsResponse::Error {
                        code: "cleanup_error".to_string(),
                        message: error.to_string(),
                    },
                })
            })
            .await;
        outcome.unwrap_or_else(|error| CommsResponse::Error {
            code: "cleanup_error".to_string(),
            message: error.to_string(),
        })
    }

    pub(super) async fn on_agents_status(&self, agent_ttl_secs: u64) -> CommsResponse {
        if !(MIN_RETENTION_SECS..=MAX_RETENTION_SECS).contains(&agent_ttl_secs) {
            return CommsResponse::Error {
                code: "invalid_retention_policy".to_string(),
                message: format!("agent_ttl_secs must be between {MIN_RETENTION_SECS} and {MAX_RETENTION_SECS}"),
            };
        }
        let ttl = std::time::Duration::from_secs(agent_ttl_secs);
        match self.store_blocking(move |store| store.agent_status(ttl)).await {
            Ok(report) => CommsResponse::AgentsStatus(report),
            Err(error) => CommsResponse::Error {
                code: "status_error".to_string(),
                message: error.to_string(),
            },
        }
    }

    pub(super) async fn on_hello(
        &self,
        agent: AgentId,
        proto_ver: u32,
        remote: Option<String>,
        cwd: Option<std::path::PathBuf>,
        session: &mut Session,
    ) -> Result<CommsResponse, CommsStoreError> {
        if proto_ver != PROTO_VER {
            return Ok(CommsResponse::Error {
                code: "proto_skew".to_string(),
                message: format!("daemon speaks proto {PROTO_VER}, client sent {proto_ver}"),
            });
        }
        session.agent = Some(agent.clone());
        session.chain = Some(build_chain(remote, cwd));

        let now = now_micros();
        self.store_write(move |store| {
            let record = match store.get_agent(&agent)? {
                Some(mut existing) => {
                    existing.last_seen = now;
                    existing
                }
                None => AgentRecord {
                    agent_id: agent,
                    card: AgentCard::default(),
                    kind: AgentKind::Other,
                    first_seen: now,
                    last_seen: now,
                },
            };
            store.put_agent(&record)
        })
        .await?;

        Ok(CommsResponse::Welcome {
            proto_ver: PROTO_VER,
            daemon_version: self.version.clone(),
        })
    }

    pub(super) async fn on_register(
        &self,
        session: &Session,
        card: AgentCard,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let now = now_micros();
        self.store_write(move |store| {
            let record = match store.get_agent(&agent)? {
                Some(mut existing) => {
                    existing.card = card;
                    existing.last_seen = now;
                    existing
                }
                None => AgentRecord {
                    agent_id: agent,
                    card,
                    kind: AgentKind::Other,
                    first_seen: now,
                    last_seen: now,
                },
            };
            store.put_agent(&record)
        })
        .await?;
        Ok(CommsResponse::Ok)
    }

    pub(super) async fn on_list_agents(&self, thread: Option<ThreadId>) -> Result<CommsResponse, CommsStoreError> {
        let agents = self
            .store_blocking(move |store| match thread {
                None => store.list_agents(),
                Some(thread) => {
                    let mut out = Vec::new();
                    for id in store.members(&thread)? {
                        if let Some(rec) = store.get_agent(&id)? {
                            out.push(rec);
                        }
                    }
                    Ok(out)
                }
            })
            .await?;
        Ok(CommsResponse::Agents(agents))
    }

    /// Start a thread addressed by at least two of subject / path / members. The creator becomes an
    /// implicit member; any explicit members are added too. Rejects fewer than two dimensions.
    pub(super) async fn on_thread_start(
        &self,
        session: &Session,
        subject: Option<String>,
        path: Option<String>,
        members: Vec<AgentId>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(creator) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let subject = subject.filter(|s| !s.is_empty());
        let path = path.filter(|p| !p.is_empty());
        if let Err(message) = validate_dimensions(subject.as_deref(), path.as_deref(), &members, &creator) {
            return Ok(CommsResponse::Error {
                code: "insufficient_dimensions".to_string(),
                message,
            });
        }

        let mut member_set: Vec<AgentId> = vec![creator.clone()];
        for m in members {
            if !member_set.contains(&m) {
                member_set.push(m);
            }
        }

        let now = now_micros();
        let id = mint_thread_id(&creator);
        let thread = Thread {
            id: id.clone(),
            subject,
            path,
            members: member_set.clone(),
            creator: creator.clone(),
            active: true,
            created_at: now,
            last_activity: 0,
        };
        let stored = thread.clone();
        self.store_write(move |store| {
            store.put_thread(&stored)?;
            for agent in &member_set {
                store.add_member(&Membership {
                    agent_id: agent.clone(),
                    thread: id.clone(),
                    created_at: now,
                })?;
            }
            Ok(())
        })
        .await?;
        self.fan_out_discovery(&thread).await;
        Ok(CommsResponse::Thread(thread))
    }

    pub(super) async fn on_thread_join(
        &self,
        session: &Session,
        thread: ThreadId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store_write(move |store| {
            let Some(mut record) = store.get_thread(&thread)? else {
                return Ok(unknown_thread(&thread));
            };
            store.add_member(&Membership {
                agent_id: agent.clone(),
                thread: thread.clone(),
                created_at: now_micros(),
            })?;
            if !record.members.contains(&agent) {
                record.members.push(agent);
                store.put_thread(&record)?;
            }
            Ok(CommsResponse::Ok)
        })
        .await
    }

    pub(super) async fn on_thread_leave(
        &self,
        session: &Session,
        thread: ThreadId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store_write(move |store| {
            store.remove_member(&thread, &agent)?;
            if let Some(mut record) = store.get_thread(&thread)? {
                record.members.retain(|m| m != &agent);
                store.put_thread(&record)?;
            }
            Ok(CommsResponse::Ok)
        })
        .await
    }

    /// List threads DISCOVERABLE to the caller: member OR cwd matches the path glob OR (when set)
    /// the subject substring filter matches. Never all threads. Archived excluded unless requested.
    pub(super) async fn on_thread_list(
        &self,
        session: &Session,
        remote: Option<String>,
        cwd: Option<std::path::PathBuf>,
        subject_contains: Option<String>,
        include_archived: bool,
    ) -> Result<CommsResponse, CommsStoreError> {
        let agent = session.agent.clone();
        let chain = build_chain(remote, cwd);
        let filter = subject_contains.filter(|s| !s.is_empty());
        let mut out = Vec::new();
        // The full thread scan runs on the blocking pool; the discoverability filter below is pure.
        for thread in self.store_blocking(|store| store.list_threads()).await? {
            if !thread.active && !include_archived {
                continue;
            }
            let is_member = agent.as_ref().is_some_and(|a| thread.members.contains(a));
            let path_hit = thread
                .path
                .as_deref()
                .is_some_and(|p| !chain.cwd.as_os_str().is_empty() && scope::path_matches(p, &chain.cwd));
            let subject_hit = match (&filter, &thread.subject) {
                (Some(needle), Some(subject)) => subject.contains(needle.as_str()),
                _ => false,
            };
            if is_member || path_hit || subject_hit {
                out.push(thread);
            }
        }
        Ok(CommsResponse::Threads(out))
    }

    pub(super) async fn on_thread_members(&self, thread: ThreadId) -> Result<CommsResponse, CommsStoreError> {
        self.store_blocking(move |store| {
            if store.get_thread(&thread)?.is_none() {
                return Ok(unknown_thread(&thread));
            }
            Ok(CommsResponse::Members {
                members: store.members(&thread)?,
            })
        })
        .await
    }

    pub(super) async fn on_thread_add_member(
        &self,
        session: &Session,
        thread: ThreadId,
        member: AgentId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store_write(move |store| {
            let Some(mut record) = store.get_thread(&thread)? else {
                return Ok(unknown_thread(&thread));
            };
            if record.creator != agent {
                return Ok(not_creator());
            }
            store.add_member(&Membership {
                agent_id: member.clone(),
                thread: thread.clone(),
                created_at: now_micros(),
            })?;
            if !record.members.contains(&member) {
                record.members.push(member);
                store.put_thread(&record)?;
            }
            Ok(CommsResponse::Ok)
        })
        .await
    }

    pub(super) async fn on_thread_remove_member(
        &self,
        session: &Session,
        thread: ThreadId,
        member: AgentId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store_write(move |store| {
            let Some(mut record) = store.get_thread(&thread)? else {
                return Ok(unknown_thread(&thread));
            };
            if record.creator != agent {
                return Ok(not_creator());
            }
            store.remove_member(&thread, &member)?;
            record.members.retain(|m| m != &member);
            store.put_thread(&record)?;
            Ok(CommsResponse::Ok)
        })
        .await
    }

    pub(super) async fn on_thread_archive(
        &self,
        session: &Session,
        thread: ThreadId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store_write(move |store| {
            let Some(mut record) = store.get_thread(&thread)? else {
                return Ok(unknown_thread(&thread));
            };
            if record.creator != agent {
                return Ok(not_creator());
            }
            record.active = false;
            store.put_thread(&record)?;
            Ok(CommsResponse::Ok)
        })
        .await
    }

    pub(super) async fn on_post(
        &self,
        session: &Session,
        thread: ThreadId,
        fields: PostFields,
    ) -> Result<CommsResponse, CommsStoreError> {
        let PostFields {
            subject,
            tags,
            reply_to,
            body,
            idempotency_key,
        } = fields;
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        if let Some(key) = idempotency_key.as_deref()
            && !store::valid_idempotency_key(key)
        {
            return Ok(CommsResponse::Error {
                code: "bad_idempotency_key".to_string(),
                message: "idempotency_key must be 1-128 printable ASCII characters".to_string(),
            });
        }
        // One write-gated pass: the seq allocation inside `store.post` and the thread-record update
        // after it stay atomic with respect to every other mutation, so ids and seqs keep their order.
        let outcome = self
            .store_write({
                let thread = thread.clone();
                move |store| {
                    if store.get_thread(&thread)?.is_none() {
                        return Ok(Err(unknown_thread(&thread)));
                    }
                    let reply_to = if let Some(reference) = reply_to {
                        let (message_id, reply_thread) = match store.resolve_message_reference(&reference)? {
                            MessageReferenceResolution::Found { message_id, thread, .. } => (message_id, thread),
                            resolution => return Ok(Err(reference_error(&reference, resolution))),
                        };
                        if !store.is_member(&reply_thread, &agent)? {
                            return Ok(Err(not_member(&reply_thread)));
                        }
                        if reply_thread != thread {
                            return Ok(Err(CommsResponse::Error {
                                code: "reply_thread_mismatch".to_string(),
                                message: "reply target belongs to another thread".to_string(),
                            }));
                        }
                        Some(message_id)
                    } else {
                        None
                    };
                    let id = mint_message_id(&thread, &agent);
                    let meta = store::build_meta(id, thread.clone(), agent, subject, tags, reply_to, &body);
                    let stored = match idempotency_key {
                        Some(key) => match store.post_keyed(&thread, meta, MessageBody(body), &key)? {
                            store::PostOutcome::Stored(_, stored) => stored,
                            store::PostOutcome::Duplicate(message_id) => {
                                return Ok(Err(CommsResponse::Posted { message_id }));
                            }
                        },
                        None => store.post(&thread, meta, MessageBody(body))?.1,
                    };
                    if let Some(mut record) = store.get_thread(&thread)? {
                        record.last_activity = stored.ts_micros;
                        store.put_thread(&record)?;
                    }
                    Ok(Ok(stored))
                }
            })
            .await?;
        let stored = match outcome {
            Ok(stored) => stored,
            Err(response) => return Ok(response),
        };
        self.fan_out(&thread, &stored).await;
        Ok(CommsResponse::Posted { message_id: stored.id })
    }

    pub(super) async fn on_history(
        &self,
        thread: ThreadId,
        cursor: Option<Cursor>,
        limit: Option<u32>,
        since_micros: Option<i64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let after = decode_after(cursor.as_ref(), thread.as_str());
        let limit = clamp_limit(limit);
        let page = {
            let thread = thread.clone();
            self.store_blocking(move |store| store.history_since(&thread, after, limit, since_micros))
                .await?
        };
        let next = page.more.then(|| Cursor::encode(thread.as_str(), page.last_seq));
        let messages = page
            .messages
            .into_iter()
            .map(|(seq, meta)| SeqMeta { seq, meta })
            .collect();
        Ok(CommsResponse::History {
            messages,
            next_cursor: next,
        })
    }

    pub(super) async fn on_get_body(
        &self,
        session: &Session,
        message_id: String,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store_blocking(move |store| {
            let (canonical_id, thread) = match store.resolve_message_reference(&message_id)? {
                MessageReferenceResolution::Found { message_id, thread, .. } => (message_id, thread),
                resolution => return Ok(reference_error(&message_id, resolution)),
            };
            if !store.is_member(&thread, &agent)? {
                return Ok(not_member(&thread));
            }
            let body = store.get_body(&canonical_id)?;
            Ok(CommsResponse::Body { body })
        })
        .await
    }

    pub(super) async fn on_inbox(
        &self,
        session: &Session,
        cursor: Option<Cursor>,
        limit: Option<u32>,
        mark_read: bool,
        since_micros: Option<i64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let limit = clamp_limit(limit);
        let resume = cursor.as_ref().and_then(|value| value.decode().ok());
        // The per-thread scans and read-cursor writes are one blocking unit. Only `mark_read`
        // mutates, so a plain inbox read (and the per-tool-call delivery probe) skips the write gate.
        let scan = move |store: &CommsStore| -> Result<CommsResponse, CommsStoreError> {
            let mut threads = store.threads_for_agent(&agent)?;
            threads.sort_by(|a, b| a.as_str().cmp(b.as_str()));

            let mut collected = Vec::with_capacity(limit);
            let mut unread_remaining = 0u32;
            let mut progress = Vec::with_capacity(threads.len());
            for thread in &threads {
                let read_seq = store.read_cursor(&agent, thread)?;
                let cursor_seq = resume
                    .as_ref()
                    .and_then(|position| {
                        position
                            .threads
                            .iter()
                            .find(|(name, _)| name == thread.as_str())
                            .map(|(_, seq)| *seq)
                            .or_else(|| (position.thread == thread.as_str()).then_some(position.seq))
                    })
                    .unwrap_or_default();
                let after = read_seq.max(cursor_seq);
                // Bound the scan: past the page limit only a count is needed, and counting an unbounded
                // backlog made every inbox read (and the per-tool-call delivery probe) cost O(backlog).
                // One row over the cap detects truncation; the count is then a lower bound.
                let scan_cap = limit.saturating_add(INBOX_UNREAD_SCAN_SLACK);
                let mut rows = store.history_with_seq(thread, after, scan_cap.saturating_add(1))?;
                let truncated = rows.len() > scan_cap;
                rows.truncate(scan_cap);
                let mut high = after;
                let mut blocked = false;
                for (seq, meta) in rows {
                    let eligible = meta.from != agent && keep_since(meta.ts_micros, since_micros);
                    if eligible && collected.len() < limit {
                        collected.push(SeqMeta { seq, meta });
                        high = seq;
                    } else if eligible {
                        unread_remaining = unread_remaining.saturating_add(1);
                        blocked = true;
                    } else if !blocked {
                        high = seq;
                    }
                }
                if truncated {
                    unread_remaining = unread_remaining.saturating_add(1);
                }
                progress.push((thread.as_str().to_string(), high));
                if mark_read && high > after {
                    store.set_read_cursor(&agent, thread, high)?;
                }
            }

            let next_cursor = (unread_remaining > 0).then(|| Cursor::encode_inbox(progress));

            Ok(CommsResponse::Inbox {
                messages: collected,
                unread: unread_remaining,
                next_cursor,
            })
        };
        if mark_read {
            self.store_write(scan).await
        } else {
            self.store_blocking(scan).await
        }
    }

    pub(super) async fn on_ack(
        &self,
        session: &Session,
        message_ids: Vec<String>,
        thread: Option<ThreadId>,
        to_seq: Option<u64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let bulk = matches!((&thread, to_seq), (Some(_), Some(_)));
        if message_ids.is_empty() && !bulk {
            return Ok(CommsResponse::Error {
                code: "empty_ack".to_string(),
                message: "ack requires message_ids or a (thread, to_seq) pair".to_string(),
            });
        }

        self.store_write(move |store| {
            let mut targets: Vec<(ThreadId, u64)> = Vec::new();
            let mut acked: u32 = 0;
            if !message_ids.is_empty() {
                for reference in &message_ids {
                    let (thread, seq) = match store.resolve_message_reference(reference)? {
                        MessageReferenceResolution::Found { thread, seq, .. } => (thread, seq),
                        resolution => return Ok(reference_error(reference, resolution)),
                    };
                    if !store.is_member(&thread, &agent)? {
                        return Ok(not_member(&thread));
                    }
                    acked = acked.saturating_add(1);
                    upsert_high(&mut targets, &thread, seq);
                }
            }
            if let (Some(thread), Some(seq)) = (thread, to_seq) {
                if !store.is_member(&thread, &agent)? {
                    return Ok(not_member(&thread));
                }
                upsert_high(&mut targets, &thread, seq);
            }

            let mut cursors_advanced: Vec<(String, u64)> = Vec::new();
            for (thread, seq) in &targets {
                let before = store.read_cursor(&agent, thread)?;
                store.set_read_cursor(&agent, thread, *seq)?;
                let after = store.read_cursor(&agent, thread)?;
                if after > before {
                    cursors_advanced.push((thread.as_str().to_string(), after));
                }
            }

            Ok(CommsResponse::Acked {
                acked,
                cursors_advanced,
            })
        })
        .await
    }

    pub(super) async fn on_subscribe(
        &self,
        session: &Session,
        thread: ThreadId,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let joined = self
            .store_write({
                let (thread, agent) = (thread.clone(), agent.clone());
                move |store| {
                    if store.get_thread(&thread)?.is_none() {
                        return Ok(Some(unknown_thread(&thread)));
                    }
                    store.add_member(&Membership {
                        agent_id: agent,
                        thread,
                        created_at: now_micros(),
                    })?;
                    Ok(None)
                }
            })
            .await?;
        if let Some(rejection) = joined {
            return Ok(rejection);
        }
        let sub = self.next_sub.fetch_add(1, Ordering::Relaxed);
        {
            let mut reg = self.registry.lock().await;
            reg.sinks.insert(
                sub,
                SubSink {
                    scope: SubScope::Thread(thread),
                    agent,
                    chain: None,
                    tx: link_tx.clone(),
                },
            );
            reg.state = LifecycleState::Active;
        }
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        Ok(CommsResponse::Subscribed { sub })
    }

    /// Open a passive, membership-routed inbox stream (see [`SubScope::Inbox`]). Unlike
    /// `on_subscribe`, this does NOT join `thread` — it only verifies the calling agent is already
    /// a member when `thread` is `Some`, so a caller cannot use it to snoop a thread it hasn't
    /// joined.
    pub(super) async fn on_subscribe_inbox(
        &self,
        session: &Session,
        thread: Option<ThreadId>,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        if let Some(thread) = thread.clone() {
            let rejection = self
                .store_blocking({
                    let agent = agent.clone();
                    move |store| {
                        if store.get_thread(&thread)?.is_none() {
                            return Ok(Some(unknown_thread(&thread)));
                        }
                        Ok(
                            (!store.members(&thread)?.contains(&agent)).then(|| CommsResponse::Error {
                                code: "not_member".to_string(),
                                message: format!("not a member of {}", thread.as_str()),
                            }),
                        )
                    }
                })
                .await?;
            if let Some(rejection) = rejection {
                return Ok(rejection);
            }
        }
        let sub = self.next_sub.fetch_add(1, Ordering::Relaxed);
        {
            let mut reg = self.registry.lock().await;
            reg.sinks.insert(
                sub,
                SubSink {
                    scope: SubScope::Inbox { thread },
                    agent,
                    chain: session.chain.clone(),
                    tx: link_tx.clone(),
                },
            );
            reg.state = LifecycleState::Active;
        }
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        Ok(CommsResponse::Subscribed { sub })
    }

    pub(super) async fn on_unsubscribe(&self, sub: u64) -> Result<CommsResponse, CommsStoreError> {
        let removed = {
            let mut reg = self.registry.lock().await;
            reg.sinks.remove(&sub)
        };
        if removed.is_some() {
            self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
            self.maybe_idle().await;
        }
        Ok(CommsResponse::Ok)
    }

    pub(super) async fn on_status(&self) -> Result<CommsResponse, CommsStoreError> {
        // Propagate rather than `unwrap_or(0)`: a status report that answers "0 threads" when the
        // store could not be read is the same defect the report is meant to expose — it looks like
        // a healthy empty broker. Failing here is what lets `comms status` and `doctor --probe`
        // tell "serving" apart from "holding the socket and refusing every request". ~keep
        let threads = self
            .store_blocking(|store| Ok(store.list_threads()?.iter().filter(|th| th.active).count()))
            .await?;
        Ok(CommsResponse::Status(StatusReport {
            pid: std::process::id(),
            version: self.version.clone(),
            build_id: crate::version::build_id().to_string(),
            proto_ver: PROTO_VER,
            uptime_secs: self.started.elapsed().as_secs(),
            threads: u32::try_from(threads).unwrap_or(u32::MAX),
            subscribers: u32::try_from(self.subscriber_count()).unwrap_or(u32::MAX),
        }))
    }

    /// Push a new message to every live sink that should wake for `thread`. [`SubScope::Thread`]
    /// sinks wake on an exact thread match (unchanged). [`SubScope::Inbox`] sinks wake when their
    /// own filter allows this thread, the poster isn't the sink's own agent (mirrors `on_inbox`'s
    /// self-exclusion), and the sink's agent is a member of `thread` — membership is read ONCE per
    /// call and reused across every inbox sink, rather than once per sink. Best-effort: a sink
    /// whose channel is full or closed is dropped; a membership-read failure is logged and treated
    /// as "no inbox sinks wake" for this post rather than failing the post itself.
    async fn fan_out(&self, thread: &ThreadId, meta: &MessageMeta) {
        let members = {
            let thread = thread.clone();
            self.store_blocking(move |store| store.members(&thread)).await
        }
        .unwrap_or_else(|error| {
            tracing::warn!(%error, thread = thread.as_str(), "comms: fan_out membership read failed");
            Vec::new()
        });
        let mut dead: Vec<u64> = Vec::new();
        {
            let reg = self.registry.lock().await;
            for (sub, sink) in reg.sinks.iter() {
                let wakes = match &sink.scope {
                    SubScope::Thread(t) => t == thread,
                    SubScope::Inbox { thread: filter } => {
                        (filter.is_none() || filter.as_ref() == Some(thread))
                            && meta.from != sink.agent
                            && members.contains(&sink.agent)
                    }
                };
                if !wakes {
                    continue;
                }
                let note = CommsOut::Notification(CommsNotification::Message(meta.clone()));
                if sink.tx.try_send(note).is_err() {
                    dead.push(*sub);
                }
            }
        }
        if !dead.is_empty() {
            let mut reg = self.registry.lock().await;
            for sub in dead {
                if reg.sinks.remove(&sub).is_some() {
                    self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Notify live unfiltered inbox waiters when a newly-created thread matches their path scope.
    /// This is deliberately metadata-only and never adds membership. The notification stream has
    /// no durable cursor; clients reconnect by listing currently discoverable threads.
    async fn fan_out_discovery(&self, thread: &Thread) {
        let Some(path) = thread.path.as_deref() else {
            return;
        };
        let mut dead = Vec::new();
        {
            let registry = self.registry.lock().await;
            for (sub, sink) in &registry.sinks {
                let SubScope::Inbox { thread: None } = &sink.scope else {
                    continue;
                };
                if thread.members.contains(&sink.agent) {
                    continue;
                }
                let Some(chain) = &sink.chain else {
                    continue;
                };
                if !scope::path_matches(path, &chain.cwd) {
                    continue;
                }
                let notification = CommsOut::Notification(CommsNotification::ThreadDiscovered(thread.clone()));
                if sink.tx.try_send(notification).is_err() {
                    dead.push(*sub);
                }
            }
        }
        if !dead.is_empty() {
            let mut registry = self.registry.lock().await;
            for sub in dead {
                if registry.sinks.remove(&sub).is_some() {
                    self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Transition to Idle when the last subscriber leaves.
    pub(super) async fn maybe_idle(&self) {
        if self.subscriber_count() == 0 {
            let mut reg = self.registry.lock().await;
            if reg.state == LifecycleState::Active {
                reg.state = LifecycleState::Idle;
                tracing::debug!("comms: broker idle (no subscribers); socket + flock retained");
            }
        }
    }
}
