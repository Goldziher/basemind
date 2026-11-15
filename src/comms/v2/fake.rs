//! An in-memory [`AgentOps`]: the executable reading of ADR-0021.
//!
//! It keeps no file, runs no daemon and takes its time from a clock the test moves, so every rule
//! (membership, read positions, authorization, replay, retention) can be asserted without
//! sleeping. It passes the [conformance suite](crate::comms::v2::conformance), which is what a store-backed
//! implementation is held to as well.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::api::{
    Actor, AgentOps, COMMS_STORE_VER, Caller, CommsError, Cursor, InboxBody, InboxMessage, InboxReply, InboxSelect,
    MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, MessageId, PostBody, PostReply, PostTarget,
    REPLAY_TTL, ReadKind, ReplayKey, RepoKey, SEEN_TTL, ThreadAction, ThreadFilter, ThreadId, ThreadListing,
    ThreadQuery, ThreadSummary, ThreadUpdate, ThreadsReply, WaitOutcome, check_body, check_recipient_count,
    deliverable, effective_limit, inline_body, message_cost, new_thread_recipients, normalize_query, normalize_subject,
    normalize_thread_path, normalize_thread_paths, path_selects, subject_matches,
};
use super::conformance::ConformanceHarness;
use crate::path::RelPath;

/// Where the fake's clock starts, in microseconds since the unix epoch. Fixed, so timestamps in
/// the fake's own assertions are exact.
pub const FAKE_EPOCH_MICROS: i64 = 1_700_000_000_000_000;

/// In-memory agent coordination with ADR-0021 semantics.
pub struct FakeAgents {
    state: Mutex<State>,
}

struct State {
    now_micros: i64,
    /// Posts stored so far: the store-wide post order, and what [`AgentOps::arrivals`] reports.
    posts: u64,
    store_ver: u32,
    next_bits: u32,
    queued_bits: VecDeque<u32>,
    threads: BTreeMap<ThreadId, ThreadState>,
    /// When each agent last made a successful call.
    seen: BTreeMap<Actor, i64>,
    post_replays: BTreeMap<(Actor, Option<ThreadId>, ReplayKey), (i64, PostReply)>,
    inbox_replays: BTreeMap<(Actor, ReplayKey), (i64, Delivered)>,
}

struct ThreadState {
    subject: String,
    repo: RepoKey,
    paths: Vec<RelPath>,
    open: bool,
    archived: bool,
    creator: Actor,
    head: u64,
    last_activity_micros: i64,
    messages: BTreeMap<u64, StoredMessage>,
    members: BTreeMap<Actor, Position>,
}

struct StoredMessage {
    from: Actor,
    ts_micros: i64,
    /// Position in the store-wide post order: what an inbox across threads sorts by.
    order: u64,
    reply_to: Option<MessageId>,
    body: String,
}

/// One row of the ADR's `members` table.
#[derive(Clone, Copy)]
struct Position {
    read_seq: u64,
    notified_seq: u64,
}

/// What a read that moved a position delivered, kept against its replay key. The range and not
/// the bodies: a replay renders it again from the store, as stored, without a second
/// authorization. A read that delivered nothing keeps nothing, so repeating it reads afresh.
#[derive(Clone)]
struct Delivered {
    ids: Vec<MessageId>,
    read_to: Vec<MessageId>,
    more: bool,
}

impl Default for FakeAgents {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeAgents {
    /// An empty store at [`COMMS_STORE_VER`], with the clock at [`FAKE_EPOCH_MICROS`].
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                now_micros: FAKE_EPOCH_MICROS,
                posts: 0,
                store_ver: COMMS_STORE_VER,
                next_bits: 0,
                queued_bits: VecDeque::new(),
                threads: BTreeMap::new(),
                seen: BTreeMap::new(),
                post_replays: BTreeMap::new(),
                inbox_replays: BTreeMap::new(),
            }),
        }
    }

    /// Draw the next thread ids from `bits` before falling back to the counter. Repeating a value
    /// reproduces an id collision.
    pub fn queue_thread_bits(&self, bits: impl IntoIterator<Item = u32>) {
        self.lock().queued_bits.extend(bits);
    }

    /// A panic in another test thread must not hide this one's assertion behind a poison error.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run one operation: against the store as retention left it, recording the caller as seen
    /// when it succeeds. A store newer than the binary is neither swept nor written to.
    fn call<T>(
        &self,
        caller: &Caller,
        operation: impl FnOnce(&mut State) -> Result<T, CommsError>,
    ) -> Result<T, CommsError> {
        let mut state = self.lock();
        let writable = state.check_writable().is_ok();
        if writable {
            state.expire();
        }
        let result = operation(&mut state);
        if writable && result.is_ok() {
            let now = state.now_micros;
            state.seen.insert(caller.actor.clone(), now);
        }
        result
    }
}

impl ConformanceHarness for FakeAgents {
    fn advance(&self, by: Duration) {
        let mut state = self.lock();
        state.now_micros = state.now_micros.saturating_add(ttl_micros(by));
    }

    fn position(&self, thread: &ThreadId, actor: &Actor) -> Option<(u64, u64)> {
        let state = self.lock();
        let position = state.threads.get(thread)?.members.get(actor)?;
        Some((position.read_seq, position.notified_seq))
    }

    fn set_store_version(&self, version: u32) {
        self.lock().store_ver = version;
    }

    fn thread_count(&self) -> usize {
        let mut state = self.lock();
        if state.check_writable().is_ok() {
            state.expire();
        }
        state.threads.len()
    }
}

impl AgentOps for FakeAgents {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        self.call(caller, |state| state.post(caller, body))
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        self.call(caller, |state| state.inbox(caller, &body))
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        self.call(caller, |state| match &query {
            ThreadQuery::Describe(id) => state.describe(caller, id),
            ThreadQuery::List(filter) => state.list(caller, filter),
        })
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        self.call(caller, |state| {
            state.check_writable()?;
            let pending = state.update(caller, &update)?;
            let thread = state.thread(&update.thread)?;
            Ok(ThreadsReply {
                threads: vec![summarize(&update.thread, thread, &caller.actor)],
                pending,
                next_cursor: None,
                notices: Vec::new(),
            })
        })
    }

    fn arrivals(&self) -> u64 {
        self.lock().posts
    }
}

impl State {
    /// Drop what retention no longer keeps: expired messages, threads left without one, replay
    /// entries past their hour, and agents that made no call for [`SEEN_TTL`].
    fn expire(&mut self) {
        let now = self.now_micros;
        let cutoff = now.saturating_sub(ttl_micros(MESSAGE_TTL));
        for thread in self.threads.values_mut() {
            thread.messages.retain(|_, message| message.ts_micros > cutoff);
        }
        self.threads.retain(|_, thread| !thread.messages.is_empty());
        self.post_replays.retain(|_, (expires, _)| *expires > now);
        self.inbox_replays.retain(|_, (expires, _)| *expires > now);
        let seen_cutoff = now.saturating_sub(ttl_micros(SEEN_TTL));
        self.seen.retain(|_, last_call| *last_call > seen_cutoff);
    }

    fn check_writable(&self) -> Result<(), CommsError> {
        if self.store_ver > COMMS_STORE_VER {
            return Err(CommsError::StoreNewer {
                store: self.store_ver,
                binary: COMMS_STORE_VER,
            });
        }
        Ok(())
    }

    fn thread(&self, id: &ThreadId) -> Result<&ThreadState, CommsError> {
        self.threads
            .get(id)
            .ok_or_else(|| CommsError::ThreadNotFound { thread: id.clone() })
    }

    fn thread_mut(&mut self, id: &ThreadId) -> Result<&mut ThreadState, CommsError> {
        self.threads
            .get_mut(id)
            .ok_or_else(|| CommsError::ThreadNotFound { thread: id.clone() })
    }

    /// The `recipients` that have not made a call, the caller aside, in id order. Being named in a
    /// `to` or an `invite` does not make an agent seen.
    fn pending(&self, actor: &Actor, recipients: &[Actor]) -> Vec<Actor> {
        let mut pending: Vec<Actor> = recipients
            .iter()
            .filter(|recipient| *recipient != actor && !self.seen.contains_key(*recipient))
            .cloned()
            .collect();
        pending.sort();
        pending.dedup();
        pending
    }

    fn fresh_thread_id(&mut self) -> ThreadId {
        loop {
            let bits = self.queued_bits.pop_front().unwrap_or_else(|| {
                let bits = self.next_bits;
                self.next_bits = self.next_bits.wrapping_add(1);
                bits
            });
            let id = ThreadId::from_bits(bits);
            if !self.threads.contains_key(&id) {
                return id;
            }
        }
    }

    fn post(&mut self, caller: &Caller, post: PostBody) -> Result<PostReply, CommsError> {
        let PostBody { target, body } = post;
        check_body(&body)?;
        self.check_writable()?;
        let actor = &caller.actor;
        let key = caller.replay_key.as_ref();
        let (thread_id, reply_to, created, pending) = match target {
            PostTarget::NewThread { subject, to, paths } => {
                if let Some(reply) = self.replayed_post(actor, None, key) {
                    return Ok(reply);
                }
                let recipients = new_thread_recipients(actor, &to)?;
                let pending = self.pending(actor, &recipients);
                let id = self.create_thread(caller, &subject, recipients, &paths)?;
                (id, None, true, pending)
            }
            PostTarget::Thread(id) => (id, None, false, Vec::new()),
            PostTarget::ReplyTo(message) => (message.thread().clone(), Some(message), false, Vec::new()),
        };
        if !created {
            if let Some(reply) = self.replayed_post(actor, Some(&thread_id), key) {
                return Ok(reply);
            }
            let thread = match (self.threads.get(&thread_id), &reply_to) {
                (Some(thread), _) => thread,
                (None, Some(message)) => return Err(CommsError::MessageNotFound { id: message.clone() }),
                (None, None) => return Err(CommsError::ThreadNotFound { thread: thread_id }),
            };
            if !thread.members.contains_key(actor) {
                return Err(CommsError::NotMember { thread: thread_id });
            }
            if let Some(message) = reply_to.as_ref().filter(|id| !thread.messages.contains_key(&id.seq())) {
                return Err(CommsError::MessageNotFound { id: message.clone() });
            }
            if thread.archived {
                return Err(CommsError::ThreadArchived { thread: thread_id });
            }
        }
        self.now_micros = self.now_micros.saturating_add(1);
        self.posts += 1;
        let (now, order) = (self.now_micros, self.posts);
        let thread = self.thread_mut(&thread_id)?;
        thread.head += 1;
        thread.last_activity_micros = now;
        thread.messages.insert(
            thread.head,
            StoredMessage {
                from: actor.clone(),
                ts_micros: now,
                order,
                reply_to,
                body,
            },
        );
        while thread.messages.len() as u64 > MAX_MESSAGES_PER_THREAD {
            thread.messages.pop_first();
        }
        let reply = PostReply {
            id: MessageId::new(thread_id.clone(), thread.head)?,
            thread: thread_id.clone(),
            created,
            pending,
            notices: Vec::new(),
        };
        if let Some(key) = key {
            let expires = now.saturating_add(ttl_micros(REPLAY_TTL));
            let scope = (!created).then_some(thread_id);
            self.post_replays
                .entry((actor.clone(), scope, key.clone()))
                .or_insert((expires, reply.clone()));
        }
        Ok(reply)
    }

    fn replayed_post(&self, actor: &Actor, thread: Option<&ThreadId>, key: Option<&ReplayKey>) -> Option<PostReply> {
        let key = (actor.clone(), thread.cloned(), key?.clone());
        self.post_replays.get(&key).map(|(_, reply)| reply.clone())
    }

    fn create_thread(
        &mut self,
        caller: &Caller,
        subject: &str,
        recipients: Vec<Actor>,
        paths: &[RelPath],
    ) -> Result<ThreadId, CommsError> {
        let subject = normalize_subject(subject)?;
        let paths = normalize_thread_paths(paths)?;
        let active = self
            .threads
            .values()
            .filter(|thread| thread.repo == caller.repo && !thread.archived)
            .count();
        if active >= MAX_ACTIVE_THREADS_PER_REPO {
            return Err(CommsError::ThreadCapReached {
                repo: caller.repo.clone(),
                cap: MAX_ACTIVE_THREADS_PER_REPO,
            });
        }
        let start = Position {
            read_seq: 0,
            notified_seq: 0,
        };
        let open = recipients.is_empty();
        let members = recipients
            .into_iter()
            .chain([caller.actor.clone()])
            .map(|member| (member, start))
            .collect();
        let id = self.fresh_thread_id();
        self.threads.insert(
            id.clone(),
            ThreadState {
                subject,
                repo: caller.repo.clone(),
                paths,
                open,
                archived: false,
                creator: caller.actor.clone(),
                head: 0,
                last_activity_micros: self.now_micros,
                messages: BTreeMap::new(),
                members,
            },
        );
        Ok(id)
    }

    fn inbox(&mut self, caller: &Caller, body: &InboxBody) -> Result<InboxReply, CommsError> {
        let kind = body.kind()?;
        let actor = &caller.actor;
        if let InboxSelect::Message(id) = &body.select {
            return Ok(reply_with(vec![self.lookup(actor, id)?], Vec::new(), false, None));
        }
        let key = caller.replay_key.as_ref().filter(|_| kind.writes());
        if kind.writes() {
            self.check_writable()?;
            let replayed = key.and_then(|key| self.inbox_replays.get(&(actor.clone(), key.clone())));
            if let Some((_, delivered)) = replayed {
                return Ok(self.render_delivered(delivered.clone()));
            }
        }
        let after = match (kind.paged(), &body.cursor) {
            (Some(read), Some(cursor)) => Some(cursor.inbox_position(actor, body.thread(), read)?),
            _ => None,
        };
        let mut candidates = self.candidates(actor, body.thread(), kind, after)?;
        let costs = candidates.iter().map(|(_, message)| message_cost(message));
        let count = deliverable(costs, effective_limit(body.limit));
        let more = count < candidates.len();
        candidates.truncate(count);
        let next_cursor = match (kind.paged(), candidates.last()) {
            (Some(read), Some((order, _))) if more => Some(Cursor::for_inbox(actor, body.thread(), read, *order)),
            _ => None,
        };
        let messages: Vec<InboxMessage> = candidates.into_iter().map(|(_, message)| message).collect();
        let read_to = self.mark(actor, kind, &messages);
        if let Some(key) = key.filter(|_| !messages.is_empty()) {
            let expires = self.now_micros.saturating_add(ttl_micros(REPLAY_TTL));
            let delivered = Delivered {
                ids: messages.iter().map(|message| message.id.clone()).collect(),
                read_to: read_to.clone(),
                more,
            };
            self.inbox_replays
                .entry((actor.clone(), key.clone()))
                .or_insert((expires, delivered));
        }
        Ok(reply_with(messages, read_to, more, next_cursor))
    }

    fn lookup(&self, actor: &Actor, id: &MessageId) -> Result<InboxMessage, CommsError> {
        let not_found = || CommsError::MessageNotFound { id: id.clone() };
        let thread = self.threads.get(id.thread()).ok_or_else(not_found)?;
        if !thread.members.contains_key(actor) {
            return Err(CommsError::NotMember {
                thread: id.thread().clone(),
            });
        }
        let message = thread.messages.get(&id.seq()).ok_or_else(not_found)?;
        Ok(render(id.clone(), thread, message, true))
    }

    /// The messages a read of `kind` could return, in store-wide post order.
    fn candidates(
        &self,
        actor: &Actor,
        only: Option<&ThreadId>,
        kind: ReadKind,
        after: Option<u64>,
    ) -> Result<Vec<(u64, InboxMessage)>, CommsError> {
        if let Some(only) = only
            && !self.thread(only)?.members.contains_key(actor)
        {
            return Err(CommsError::NotMember { thread: only.clone() });
        }
        let history = kind == ReadKind::History;
        let mut candidates = Vec::new();
        for (id, thread) in &self.threads {
            let selected = only.is_none_or(|only| only == id);
            let Some(position) = thread.members.get(actor).filter(|_| selected) else {
                continue;
            };
            let floor = match kind {
                ReadKind::History | ReadKind::ById => 0,
                ReadKind::Announcing => position.read_seq.max(position.notified_seq),
                ReadKind::Marking | ReadKind::Peek => position.read_seq,
            };
            for (seq, message) in thread.messages.range(floor + 1..) {
                let own = !history && message.from == *actor;
                let paged_out = after.is_some_and(|after| message.order <= after);
                if !own && !paged_out {
                    let id = MessageId::new(id.clone(), *seq)?;
                    candidates.push((message.order, render(id, thread, message, false)));
                }
            }
        }
        candidates.sort_by_key(|(order, _)| *order);
        Ok(candidates)
    }

    /// Move the positions a read of `kind` moves, to the last of `delivered` in each thread.
    /// Returns the new read positions. `delivered` is a prefix of the unread messages, so nothing
    /// unread is ever stepped over.
    fn mark(&mut self, actor: &Actor, kind: ReadKind, delivered: &[InboxMessage]) -> Vec<MessageId> {
        let mut read_to = Vec::new();
        if !kind.writes() {
            return read_to;
        }
        let mut last_delivered: BTreeMap<&ThreadId, &MessageId> = BTreeMap::new();
        for message in delivered {
            let last = last_delivered.entry(message.id.thread()).or_insert(&message.id);
            if message.id.seq() > last.seq() {
                *last = &message.id;
            }
        }
        for (thread_id, last) in last_delivered {
            let position = self
                .threads
                .get_mut(thread_id)
                .and_then(|thread| thread.members.get_mut(actor));
            let Some(position) = position else {
                continue;
            };
            let seq = last.seq();
            position.notified_seq = position.notified_seq.max(seq);
            if kind == ReadKind::Marking && seq > position.read_seq {
                position.read_seq = seq;
                read_to.push(last.clone());
            }
        }
        read_to
    }

    /// Rebuild a reply from a delivered range. A message that expired since is left out.
    fn render_delivered(&self, delivered: Delivered) -> InboxReply {
        let messages = delivered
            .ids
            .into_iter()
            .filter_map(|id| {
                let thread = self.threads.get(id.thread())?;
                let message = thread.messages.get(&id.seq())?;
                Some(render(id, thread, message, false))
            })
            .collect();
        reply_with(messages, delivered.read_to, delivered.more, None)
    }

    fn describe(&self, caller: &Caller, id: &ThreadId) -> Result<ThreadsReply, CommsError> {
        let thread = self.thread(id)?;
        let member = thread.members.contains_key(&caller.actor);
        if !member && !(thread.open && thread.repo == caller.repo) {
            return Err(CommsError::ThreadClosed { thread: id.clone() });
        }
        Ok(ThreadsReply {
            threads: vec![summarize(id, thread, &caller.actor)],
            pending: Vec::new(),
            next_cursor: None,
            notices: Vec::new(),
        })
    }

    fn list(&self, caller: &Caller, filter: &ThreadFilter) -> Result<ThreadsReply, CommsError> {
        let actor = &caller.actor;
        let query = filter.query.as_deref().map(normalize_query).transpose()?;
        let path = filter.path.as_ref().map(normalize_thread_path).transpose()?;
        let listing = ThreadListing {
            actor,
            repo: &caller.repo,
            query: query.as_deref(),
            path: path.as_ref(),
            archived: filter.archived,
        };
        let after = filter
            .cursor
            .as_ref()
            .map(|cursor| cursor.threads_position(&listing))
            .transpose()?;
        let mut matching: Vec<(&ThreadId, &ThreadState)> = self
            .threads
            .iter()
            .filter(|(_, thread)| {
                let visible = thread.members.contains_key(actor)
                    || (filter.discovers() && thread.open && thread.repo == caller.repo);
                let matches = query
                    .as_deref()
                    .is_none_or(|query| subject_matches(&thread.subject, query))
                    && path.as_ref().is_none_or(|path| path_selects(path, &thread.paths));
                visible && matches && (filter.archived || !thread.archived)
            })
            .collect();
        matching.sort_by(|(id, thread), (other_id, other)| {
            other
                .last_activity_micros
                .cmp(&thread.last_activity_micros)
                .then_with(|| id.cmp(other_id))
        });
        if let Some((activity, after_id)) = &after {
            matching.retain(|(id, thread)| {
                thread.last_activity_micros < *activity || (thread.last_activity_micros == *activity && *id > after_id)
            });
        }
        let limit = effective_limit(filter.limit);
        let next_cursor = match matching.get(limit.saturating_sub(1)) {
            Some((id, thread)) if matching.len() > limit => {
                Some(Cursor::for_threads(&listing, thread.last_activity_micros, id))
            }
            _ => None,
        };
        Ok(ThreadsReply {
            threads: matching
                .into_iter()
                .take(limit)
                .map(|(id, thread)| summarize(id, thread, actor))
                .collect(),
            pending: Vec::new(),
            next_cursor,
            notices: Vec::new(),
        })
    }

    /// Apply one membership or lifecycle change; returns the invited agents not seen.
    fn update(&mut self, caller: &Caller, update: &ThreadUpdate) -> Result<Vec<Actor>, CommsError> {
        let (actor, id) = (&caller.actor, &update.thread);
        let pending = match &update.action {
            ThreadAction::Invite(invited) => self.pending(actor, invited),
            ThreadAction::Join | ThreadAction::Leave | ThreadAction::Archive => Vec::new(),
        };
        let thread = self.thread_mut(id)?;
        let member = thread.members.contains_key(actor);
        let at_head = Position {
            read_seq: thread.head,
            notified_seq: thread.head,
        };
        match &update.action {
            ThreadAction::Join if member => {}
            ThreadAction::Join if !thread.open || thread.repo != caller.repo => {
                return Err(CommsError::ThreadClosed { thread: id.clone() });
            }
            ThreadAction::Join if thread.archived => return Err(CommsError::ThreadArchived { thread: id.clone() }),
            ThreadAction::Join => {
                thread.members.insert(actor.clone(), at_head);
            }
            ThreadAction::Leave | ThreadAction::Invite(_) if !member => {
                return Err(CommsError::NotMember { thread: id.clone() });
            }
            ThreadAction::Leave => {
                thread.members.remove(actor);
                if thread.members.is_empty() {
                    thread.archived = true;
                }
            }
            ThreadAction::Invite(invited) if invited.is_empty() => return Err(CommsError::EmptyInvite),
            ThreadAction::Invite(invited) => {
                check_recipient_count("invite", invited)?;
                if thread.archived {
                    return Err(CommsError::ThreadArchived { thread: id.clone() });
                }
                for agent in invited {
                    thread.members.entry(agent.clone()).or_insert(at_head);
                }
            }
            ThreadAction::Archive if thread.creator != *actor => {
                return Err(CommsError::NotCreator { thread: id.clone() });
            }
            ThreadAction::Archive => thread.archived = true,
        }
        Ok(pending)
    }
}

fn ttl_micros(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX)
}

fn reply_with(
    messages: Vec<InboxMessage>,
    read_to: Vec<MessageId>,
    more: bool,
    next_cursor: Option<Cursor>,
) -> InboxReply {
    InboxReply {
        messages,
        read_to,
        more,
        next_cursor,
        notices: Vec::new(),
        wait: WaitOutcome::NotWaited,
    }
}

fn render(id: MessageId, thread: &ThreadState, message: &StoredMessage, whole: bool) -> InboxMessage {
    let (body, truncated) = if whole {
        (message.body.as_str(), false)
    } else {
        inline_body(&message.body)
    };
    InboxMessage {
        id,
        subject: thread.subject.clone(),
        from: message.from.clone(),
        ts_micros: message.ts_micros,
        reply_to: message.reply_to.clone(),
        body: body.to_string(),
        truncated,
    }
}

fn summarize(id: &ThreadId, thread: &ThreadState, actor: &Actor) -> ThreadSummary {
    let position = thread.members.get(actor);
    let unread = position.map_or(0, |position| {
        thread
            .messages
            .range(position.read_seq + 1..)
            .filter(|(_, message)| message.from != *actor)
            .count() as u64
    });
    ThreadSummary {
        id: id.clone(),
        subject: thread.subject.clone(),
        repo: thread.repo.clone(),
        paths: thread.paths.clone(),
        open: thread.open,
        archived: thread.archived,
        creator: thread.creator.clone(),
        member: position.is_some(),
        unread,
        head: thread.head,
        last_activity_micros: thread.last_activity_micros,
        peers: thread
            .members
            .keys()
            .filter(|member| *member != actor)
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod tests;
