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
use super::daemon::{Broker, DEFAULT_LIMIT, LifecycleState, MAX_LIMIT, Session, SubScope, SubSink};
use super::ids::{AgentId, ThreadId};
use super::model::{AgentCard, AgentKind, AgentRecord, Membership, MessageBody, MessageMeta, Thread, now_micros};
use super::protocol::{CommsNotification, CommsOut, CommsResponse, PROTO_VER, SeqMeta, StatusReport};
use super::scope;
use super::store::{self, CommsStoreError};

impl Broker {
    pub(super) fn on_hello(
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
        let record = match self.store.get_agent(&agent)? {
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
        self.store.put_agent(&record)?;

        Ok(CommsResponse::Welcome {
            proto_ver: PROTO_VER,
            daemon_version: self.version.clone(),
        })
    }

    pub(super) fn on_register(&self, session: &Session, card: AgentCard) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let now = now_micros();
        let record = match self.store.get_agent(&agent)? {
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
        self.store.put_agent(&record)?;
        Ok(CommsResponse::Ok)
    }

    pub(super) fn on_list_agents(&self, thread: Option<ThreadId>) -> Result<CommsResponse, CommsStoreError> {
        let agents = match thread {
            None => self.store.list_agents()?,
            Some(thread) => {
                let members = self.store.members(&thread)?;
                let mut out = Vec::new();
                for id in members {
                    if let Some(rec) = self.store.get_agent(&id)? {
                        out.push(rec);
                    }
                }
                out
            }
        };
        Ok(CommsResponse::Agents(agents))
    }

    /// Start a thread addressed by at least two of subject / path / members. The creator becomes an
    /// implicit member; any explicit members are added too. Rejects fewer than two dimensions.
    pub(super) fn on_thread_start(
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

        // The full member set: the creator plus any explicit members, deduplicated.
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
        self.store.put_thread(&thread)?;
        for agent in &member_set {
            self.store.add_member(&Membership {
                agent_id: agent.clone(),
                thread: id.clone(),
                created_at: now,
            })?;
        }
        Ok(CommsResponse::Thread(thread))
    }

    pub(super) fn on_thread_join(&self, session: &Session, thread: ThreadId) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        self.store.add_member(&Membership {
            agent_id: agent.clone(),
            thread: thread.clone(),
            created_at: now_micros(),
        })?;
        if !record.members.contains(&agent) {
            record.members.push(agent);
            self.store.put_thread(&record)?;
        }
        Ok(CommsResponse::Ok)
    }

    pub(super) fn on_thread_leave(
        &self,
        session: &Session,
        thread: ThreadId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store.remove_member(&thread, &agent)?;
        if let Some(mut record) = self.store.get_thread(&thread)? {
            record.members.retain(|m| m != &agent);
            self.store.put_thread(&record)?;
        }
        Ok(CommsResponse::Ok)
    }

    /// List threads DISCOVERABLE to the caller: member OR cwd matches the path glob OR (when set)
    /// the subject substring filter matches. Never all threads. Archived excluded unless requested.
    pub(super) fn on_thread_list(
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
        for thread in self.store.list_threads()? {
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

    pub(super) fn on_thread_members(&self, thread: ThreadId) -> Result<CommsResponse, CommsStoreError> {
        if self.store.get_thread(&thread)?.is_none() {
            return Ok(unknown_thread(&thread));
        }
        Ok(CommsResponse::Members {
            members: self.store.members(&thread)?,
        })
    }

    pub(super) fn on_thread_add_member(
        &self,
        session: &Session,
        thread: ThreadId,
        member: AgentId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        if record.creator != agent {
            return Ok(not_creator());
        }
        self.store.add_member(&Membership {
            agent_id: member.clone(),
            thread: thread.clone(),
            created_at: now_micros(),
        })?;
        if !record.members.contains(&member) {
            record.members.push(member);
            self.store.put_thread(&record)?;
        }
        Ok(CommsResponse::Ok)
    }

    pub(super) fn on_thread_remove_member(
        &self,
        session: &Session,
        thread: ThreadId,
        member: AgentId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        if record.creator != agent {
            return Ok(not_creator());
        }
        self.store.remove_member(&thread, &member)?;
        record.members.retain(|m| m != &member);
        self.store.put_thread(&record)?;
        Ok(CommsResponse::Ok)
    }

    pub(super) fn on_thread_archive(
        &self,
        session: &Session,
        thread: ThreadId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        if record.creator != agent {
            return Ok(not_creator());
        }
        record.active = false;
        self.store.put_thread(&record)?;
        Ok(CommsResponse::Ok)
    }

    pub(super) async fn on_post(
        &self,
        session: &Session,
        thread: ThreadId,
        subject: String,
        tags: Vec<String>,
        reply_to: Option<String>,
        body: Vec<u8>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        if self.store.get_thread(&thread)?.is_none() {
            return Ok(unknown_thread(&thread));
        }
        let id = mint_message_id(&thread, &agent);
        let meta = store::build_meta(id, thread.clone(), agent, subject, tags, reply_to, &body);
        let (_, stored) = self.store.post(&thread, meta, MessageBody(body))?;
        if let Some(mut record) = self.store.get_thread(&thread)? {
            record.last_activity = stored.ts_micros;
            self.store.put_thread(&record)?;
        }
        self.fan_out(&thread, &stored).await;
        Ok(CommsResponse::Posted { message_id: stored.id })
    }

    pub(super) fn on_history(
        &self,
        thread: ThreadId,
        cursor: Option<Cursor>,
        limit: Option<u32>,
        since_micros: Option<i64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let after = decode_after(cursor.as_ref(), thread.as_str());
        let limit = clamp_limit(limit);
        let page = self.store.history(&thread, after, limit)?;
        let next = page.more.then(|| Cursor::encode(thread.as_str(), page.last_seq));
        let messages = page
            .messages
            .into_iter()
            .filter(|(_, meta)| keep_since(meta.ts_micros, since_micros))
            .map(|(seq, meta)| SeqMeta { seq, meta })
            .collect();
        Ok(CommsResponse::History {
            messages,
            next_cursor: next,
        })
    }

    pub(super) fn on_get_body(&self, message_id: String) -> Result<CommsResponse, CommsStoreError> {
        let body = self.store.get_body(&message_id)?;
        Ok(CommsResponse::Body { body })
    }

    pub(super) fn on_inbox(
        &self,
        session: &mut Session,
        cursor: Option<Cursor>,
        limit: Option<u32>,
        mark_read: bool,
        since_micros: Option<i64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let limit = clamp_limit(limit);
        let resume = cursor.as_ref().and_then(|c| c.decode().ok());
        let mut threads = self.store.threads_for_agent(&agent)?;
        threads.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut collected: Vec<SeqMeta> = Vec::new();
        let mut delivered_high: Vec<(ThreadId, u64)> = Vec::new();
        let mut unread_remaining: u32 = 0;
        let mut next_cursor: Option<Cursor> = None;

        for thread in &threads {
            let read_seq = self.store.read_cursor(&agent, thread)?;
            let after = match &resume {
                Some(pos) if pos.thread == thread.as_str() => pos.seq.max(read_seq),
                _ => read_seq,
            };
            let remaining = limit.saturating_sub(collected.len());
            let want = remaining.saturating_add(1).max(1);
            let rows = self.store.history_with_seq(thread, after, want)?;
            for (seq, meta) in rows {
                if meta.from == agent || !keep_since(meta.ts_micros, since_micros) {
                    upsert_high(&mut delivered_high, thread, seq);
                    continue;
                }
                if collected.len() < limit {
                    collected.push(SeqMeta { seq, meta });
                    upsert_high(&mut delivered_high, thread, seq);
                } else {
                    unread_remaining = unread_remaining.saturating_add(1);
                    if next_cursor.is_none() {
                        let resume_seq = highest_for(&delivered_high, thread).unwrap_or(after);
                        next_cursor = Some(Cursor::encode(thread.as_str(), resume_seq));
                    }
                }
            }
        }

        if mark_read {
            for (thread, seq) in &delivered_high {
                self.store.set_read_cursor(&agent, thread, *seq)?;
            }
        }

        Ok(CommsResponse::Inbox {
            messages: collected,
            unread: unread_remaining,
            next_cursor,
        })
    }

    pub(super) fn on_ack(
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

        let mut targets: Vec<(ThreadId, u64)> = Vec::new();
        let mut acked: u32 = 0;
        if !message_ids.is_empty() {
            for (_, thread, seq) in self.store.resolve_ids(&message_ids)? {
                acked = acked.saturating_add(1);
                upsert_high(&mut targets, &thread, seq);
            }
        }
        if let (Some(thread), Some(seq)) = (thread, to_seq) {
            upsert_high(&mut targets, &thread, seq);
        }

        let mut cursors_advanced: Vec<(String, u64)> = Vec::new();
        for (thread, seq) in &targets {
            let before = self.store.read_cursor(&agent, thread)?;
            self.store.set_read_cursor(&agent, thread, *seq)?;
            let after = self.store.read_cursor(&agent, thread)?;
            if after > before {
                cursors_advanced.push((thread.as_str().to_string(), after));
            }
        }

        Ok(CommsResponse::Acked {
            acked,
            cursors_advanced,
        })
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
        if self.store.get_thread(&thread)?.is_none() {
            return Ok(unknown_thread(&thread));
        }
        self.store.add_member(&Membership {
            agent_id: agent.clone(),
            thread: thread.clone(),
            created_at: now_micros(),
        })?;
        let sub = self.next_sub.fetch_add(1, Ordering::Relaxed);
        {
            let mut reg = self.registry.lock().await;
            reg.sinks.insert(
                sub,
                SubSink {
                    scope: SubScope::Thread(thread),
                    agent,
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
        if let Some(thread) = &thread {
            if self.store.get_thread(thread)?.is_none() {
                return Ok(unknown_thread(thread));
            }
            if !self.store.members(thread)?.contains(&agent) {
                return Ok(CommsResponse::Error {
                    code: "not_member".to_string(),
                    message: format!("not a member of {}", thread.as_str()),
                });
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

    pub(super) async fn on_status(&self) -> CommsResponse {
        let threads = self
            .store
            .list_threads()
            .map(|t| t.iter().filter(|th| th.active).count())
            .unwrap_or(0);
        CommsResponse::Status(StatusReport {
            pid: std::process::id(),
            version: self.version.clone(),
            proto_ver: PROTO_VER,
            uptime_secs: self.started.elapsed().as_secs(),
            threads: u32::try_from(threads).unwrap_or(u32::MAX),
            subscribers: u32::try_from(self.subscriber_count()).unwrap_or(u32::MAX),
        })
    }

    /// Push a new message to every live sink that should wake for `thread`. [`SubScope::Thread`]
    /// sinks wake on an exact thread match (unchanged). [`SubScope::Inbox`] sinks wake when their
    /// own filter allows this thread, the poster isn't the sink's own agent (mirrors `on_inbox`'s
    /// self-exclusion), and the sink's agent is a member of `thread` — membership is read ONCE per
    /// call and reused across every inbox sink, rather than once per sink. Best-effort: a sink
    /// whose channel is full or closed is dropped; a membership-read failure is logged and treated
    /// as "no inbox sinks wake" for this post rather than failing the post itself.
    async fn fan_out(&self, thread: &ThreadId, meta: &MessageMeta) {
        let members = self.store.members(thread).unwrap_or_else(|error| {
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

    /// Transition to Idle when the last subscriber leaves.
    async fn maybe_idle(&self) {
        if self.subscriber_count() == 0 {
            let mut reg = self.registry.lock().await;
            if reg.state == LifecycleState::Active {
                reg.state = LifecycleState::Idle;
                tracing::debug!("comms: broker idle (no subscribers); socket + flock retained");
            }
        }
    }
}

fn need_hello() -> CommsResponse {
    CommsResponse::Error {
        code: "no_hello".to_string(),
        message: "send Hello before any other request".to_string(),
    }
}

fn unknown_thread(thread: &ThreadId) -> CommsResponse {
    CommsResponse::Error {
        code: "unknown_thread".to_string(),
        message: format!("no thread {}", thread.as_str()),
    }
}

fn not_creator() -> CommsResponse {
    CommsResponse::Error {
        code: "not_creator".to_string(),
        message: "only the thread creator may manage membership or archive it".to_string(),
    }
}

fn clamp_limit(limit: Option<u32>) -> usize {
    usize::try_from(limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)).unwrap_or(DEFAULT_LIMIT as usize)
}

fn decode_after(cursor: Option<&Cursor>, thread: &str) -> u64 {
    match cursor.and_then(|c| c.decode().ok()) {
        Some(pos) if pos.thread == thread || pos.thread.is_empty() => pos.seq,
        _ => 0,
    }
}

/// Whether a message with `ts_micros` passes the optional recency cutoff.
fn keep_since(ts_micros: i64, since_micros: Option<i64>) -> bool {
    match since_micros {
        Some(cut) => ts_micros >= cut,
        None => true,
    }
}

/// Record the highest delivered `seq` for `thread` in a small per-page accumulator.
fn upsert_high(acc: &mut Vec<(ThreadId, u64)>, thread: &ThreadId, seq: u64) {
    if let Some(entry) = acc.iter_mut().find(|(t, _)| t == thread) {
        if seq > entry.1 {
            entry.1 = seq;
        }
    } else {
        acc.push((thread.clone(), seq));
    }
}

/// Look up the highest delivered `seq` recorded for `thread`.
fn highest_for(acc: &[(ThreadId, u64)], thread: &ThreadId) -> Option<u64> {
    acc.iter().find(|(t, _)| t == thread).map(|(_, s)| *s)
}
