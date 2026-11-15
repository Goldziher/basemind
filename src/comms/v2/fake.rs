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
    Actor, AgentOps, COMMS_STORE_VER, Caller, CommsError, InboxBody, InboxMessage, InboxReply, InboxSelect,
    MAX_MESSAGES_PER_THREAD, MessageId, PostBody, PostReply, PostTarget, ReadKind, ReplayKey, RepoKey, ThreadAction,
    ThreadFilter, ThreadId, ThreadQuery, ThreadSummary, ThreadUpdate, ThreadsReply, check_body,
};
use super::conformance::ConformanceHarness;
use super::rules::{self, Access, Change, Delivered, NewThread, Position, Selection};
use crate::path::RelPath;

/// Where the fake's clock starts, in microseconds since the unix epoch. Fixed, so timestamps in
/// the fake's own assertions are exact.
pub const FAKE_EPOCH_MICROS: i64 = 1_700_000_000_000_000;

/// In-memory agent coordination with ADR-0021 semantics.
pub struct FakeAgents {
    state: Mutex<State>,
}

struct State {
    /// What the clock reads: what everything that expires is judged against.
    now_micros: i64,
    /// The timestamp of the previous post, which [`rules::post_timestamp`] stamps the next from.
    last_ts: i64,
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
                last_ts: i64::MIN,
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

    /// Remove everything that has expired at the clock's reading, as a store's sweep does once
    /// it has nothing left to remove. No call does this by itself: what has expired is left out
    /// by the reads, so it is back when the clock is, unless this ran in between. A store newer
    /// than the binary is not swept.
    pub fn sweep(&self) {
        let mut state = self.lock();
        if state.check_writable().is_ok() {
            state.sweep();
        }
    }

    /// Run one operation, recording the caller as seen when it succeeds and its record is due.
    /// A store newer than the binary is not written to.
    fn call<T>(
        &self,
        caller: &Caller,
        operation: impl FnOnce(&mut State) -> Result<T, CommsError>,
    ) -> Result<T, CommsError> {
        let mut state = self.lock();
        let writable = state.check_writable().is_ok();
        let result = operation(&mut state);
        if writable && result.is_ok() {
            let now = state.now_micros;
            let recorded = state.seen.get(&caller.actor).copied();
            if !rules::seen_record_is_current(recorded, now) {
                state.seen.insert(caller.actor.clone(), now);
            }
        }
        result
    }
}

impl ConformanceHarness for FakeAgents {
    fn advance(&self, by: Duration) {
        let mut state = self.lock();
        state.now_micros = state.now_micros.saturating_add(rules::ttl_micros(by));
    }

    fn rewind(&self, by: Duration) {
        let mut state = self.lock();
        state.now_micros = state.now_micros.saturating_sub(rules::ttl_micros(by));
    }

    fn position(&self, thread: &ThreadId, actor: &Actor) -> Option<(u64, u64)> {
        let state = self.lock();
        let position = state.kept_thread(thread)?.members.get(actor)?;
        Some((position.read_seq, position.notified_seq))
    }

    fn set_store_version(&self, version: u32) {
        self.lock().store_ver = version;
    }

    fn sweep(&self) {
        FakeAgents::sweep(self);
    }

    fn thread_count(&self) -> usize {
        FakeAgents::sweep(self);
        self.lock().threads.len()
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
                threads: vec![summarize(&update.thread, thread, &caller.actor, state.now_micros)],
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
    /// Drop what retention no longer keeps: expired threads with every message of theirs,
    /// expired messages of the threads that stay, replay entries past their hour, and seen
    /// records that expired ([`rules::seen_expired`]).
    fn sweep(&mut self) {
        let now = self.now_micros;
        self.threads
            .retain(|_, thread| !rules::message_expired(thread.last_activity_micros, now));
        for thread in self.threads.values_mut() {
            thread
                .messages
                .retain(|_, message| !rules::message_expired(message.ts_micros, now));
        }
        self.post_replays
            .retain(|_, (expires, _)| !rules::replay_expired(*expires, now));
        self.inbox_replays
            .retain(|_, (expires, _)| !rules::replay_expired(*expires, now));
        self.seen.retain(|_, last_call| !rules::seen_expired(*last_call, now));
    }

    fn check_writable(&self) -> Result<(), CommsError> {
        rules::check_writable(self.store_ver)
    }

    /// The thread `id`, unless it has expired with the last message posted to it. Everything
    /// but the draw of a new id goes through this and not through `threads`, which holds expired
    /// threads until [`State::sweep`] runs.
    fn kept_thread(&self, id: &ThreadId) -> Option<&ThreadState> {
        self.threads
            .get(id)
            .filter(|thread| !rules::message_expired(thread.last_activity_micros, self.now_micros))
    }

    /// Message `seq` of `thread`, unless it has expired.
    fn kept_message<'a>(&self, thread: &'a ThreadState, seq: u64) -> Option<&'a StoredMessage> {
        thread
            .messages
            .get(&seq)
            .filter(|message| !rules::message_expired(message.ts_micros, self.now_micros))
    }

    fn thread(&self, id: &ThreadId) -> Result<&ThreadState, CommsError> {
        self.kept_thread(id)
            .ok_or_else(|| CommsError::ThreadNotFound { thread: id.clone() })
    }

    fn thread_mut(&mut self, id: &ThreadId) -> Result<&mut ThreadState, CommsError> {
        self.thread(id)?;
        self.threads
            .get_mut(id)
            .ok_or_else(|| CommsError::ThreadNotFound { thread: id.clone() })
    }

    /// The `recipients` that have not made a call, the caller aside, in id order. Being named in a
    /// `to` or an `invite` does not make an agent seen.
    fn pending(&self, actor: &Actor, recipients: &[Actor]) -> Vec<Actor> {
        let mut pending = rules::addressed(actor, recipients);
        pending.retain(|recipient| {
            let last_call = self.seen.get(recipient);
            last_call.is_none_or(|last_call| rules::seen_expired(*last_call, self.now_micros))
        });
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
                let plan = NewThread::plan(actor, &subject, &to, &paths)?;
                let pending = self.pending(actor, &plan.recipients);
                let id = self.create_thread(caller, plan)?;
                (id, None, true, pending)
            }
            PostTarget::Thread(id) => (id, None, false, Vec::new()),
            PostTarget::ReplyTo(message) => (message.thread().clone(), Some(message), false, Vec::new()),
        };
        if !created {
            if let Some(reply) = self.replayed_post(actor, Some(&thread_id), key) {
                return Ok(reply);
            }
            let thread = self.kept_thread(&thread_id);
            let access = thread.map(|thread| thread.access(caller));
            let reply_target_stored = match (thread, &reply_to) {
                (Some(thread), Some(message)) => self.kept_message(thread, message.seq()).is_some(),
                _ => true,
            };
            rules::authorize_post(&thread_id, access.as_ref(), reply_to.as_ref(), reply_target_stored)?;
        }
        let ts_micros = rules::post_timestamp(self.now_micros, self.last_ts);
        self.last_ts = ts_micros;
        self.posts += 1;
        let (now, order) = (self.now_micros, self.posts);
        let thread = self.thread_mut(&thread_id)?;
        thread.head += 1;
        thread.last_activity_micros = ts_micros;
        thread.messages.insert(
            thread.head,
            StoredMessage {
                from: actor.clone(),
                ts_micros,
                order,
                reply_to,
                body,
            },
        );
        if let Some(evicted) = thread.head.checked_sub(MAX_MESSAGES_PER_THREAD) {
            thread.messages.remove(&evicted);
        }
        let reply = PostReply {
            id: MessageId::new(thread_id.clone(), thread.head)?,
            thread: thread_id.clone(),
            created,
            pending,
            notices: Vec::new(),
        };
        if let Some(key) = key {
            let scope = (!created).then_some(thread_id);
            self.post_replays.insert(
                (actor.clone(), scope, key.clone()),
                (rules::replay_expiry(now), reply.clone()),
            );
        }
        Ok(reply)
    }

    fn replayed_post(&self, actor: &Actor, thread: Option<&ThreadId>, key: Option<&ReplayKey>) -> Option<PostReply> {
        let key = (actor.clone(), thread.cloned(), key?.clone());
        let (expires, reply) = self.post_replays.get(&key)?;
        (!rules::replay_expired(*expires, self.now_micros)).then(|| reply.clone())
    }

    fn create_thread(&mut self, caller: &Caller, plan: NewThread) -> Result<ThreadId, CommsError> {
        let now = self.now_micros;
        let active = self
            .threads
            .values()
            .filter(|thread| thread.repo == caller.repo && !thread.archived)
            .filter(|thread| !rules::message_expired(thread.last_activity_micros, now))
            .count();
        rules::check_thread_cap(&caller.repo, active)?;
        let members = plan
            .recipients
            .into_iter()
            .chain([caller.actor.clone()])
            .map(|member| (member, Position::START))
            .collect();
        let id = self.fresh_thread_id();
        self.threads.insert(
            id.clone(),
            ThreadState {
                subject: plan.subject,
                repo: caller.repo.clone(),
                paths: plan.paths,
                open: plan.open,
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
            return Ok(rules::inbox_reply(
                vec![self.lookup(caller, id)?],
                Vec::new(),
                false,
                None,
            ));
        }
        let key = caller.replay_key.as_ref().filter(|_| kind.writes());
        if kind.writes() {
            self.check_writable()?;
            let replayed = key
                .and_then(|key| self.inbox_replays.get(&(actor.clone(), key.clone())))
                .filter(|(expires, _)| !rules::replay_expired(*expires, self.now_micros));
            if let Some((_, delivered)) = replayed {
                return Ok(self.render_delivered(delivered.clone()));
            }
        }
        let after = rules::cursor_position(kind, body.cursor.as_ref(), actor, body.thread())?;
        let candidates = self.candidates(caller, body.thread(), kind, after)?;
        let (page, more) = rules::take_page(candidates.into_iter().map(Ok::<_, CommsError>), body.limit)?;
        let next_cursor = rules::page_cursor(kind, &page, more, actor, body.thread());
        let messages: Vec<InboxMessage> = page.into_iter().map(|(_, message)| message).collect();
        let read_to = self.mark(actor, kind, &messages);
        if let (Some(key), Some(delivered)) = (key, Delivered::of(&messages, &read_to, more)) {
            let expires = rules::replay_expiry(self.now_micros);
            self.inbox_replays
                .insert((actor.clone(), key.clone()), (expires, delivered));
        }
        Ok(rules::inbox_reply(messages, read_to, more, next_cursor))
    }

    fn lookup(&self, caller: &Caller, id: &MessageId) -> Result<InboxMessage, CommsError> {
        let thread = self.kept_thread(id.thread());
        let access = thread.map(|thread| thread.access(caller));
        let message = thread.and_then(|thread| Some((thread, self.kept_message(thread, id.seq())?)));
        rules::authorize_lookup(id, access.as_ref(), message.is_some())?;
        let (thread, message) = message.ok_or_else(|| CommsError::MessageNotFound { id: id.clone() })?;
        Ok(thread.render(id.clone(), message, true))
    }

    /// The messages a read of `kind` could return, in store-wide post order.
    fn candidates(
        &self,
        caller: &Caller,
        only: Option<&ThreadId>,
        kind: ReadKind,
        after: Option<u64>,
    ) -> Result<Vec<(u64, InboxMessage)>, CommsError> {
        let actor = &caller.actor;
        let now = self.now_micros;
        if let Some(only) = only {
            let access = self.kept_thread(only).map(|thread| thread.access(caller));
            rules::authorize_read(only, access.as_ref())?;
        }
        let mut candidates = Vec::new();
        for (id, thread) in &self.threads {
            let selected = only.is_none_or(|only| only == id) && self.kept_thread(id).is_some();
            let Some(position) = thread.members.get(actor).filter(|_| selected) else {
                continue;
            };
            for (seq, message) in thread.messages.range(position.floor(kind) + 1..) {
                let paged_out = after.is_some_and(|after| message.order <= after);
                let expired = rules::message_expired(message.ts_micros, now);
                if rules::is_candidate(kind, &message.from, actor) && !paged_out && !expired {
                    let id = MessageId::new(id.clone(), *seq)?;
                    candidates.push((message.order, thread.render(id, message, false)));
                }
            }
        }
        candidates.sort_by_key(|(order, _)| *order);
        Ok(candidates)
    }

    /// Move the positions a read of `kind` moves, to the last of `delivered` in each thread.
    /// Returns the new read positions.
    fn mark(&mut self, actor: &Actor, kind: ReadKind, delivered: &[InboxMessage]) -> Vec<MessageId> {
        let mut read_to = Vec::new();
        if !kind.writes() {
            return read_to;
        }
        for (thread_id, last) in rules::last_delivered(delivered) {
            let position = self
                .threads
                .get_mut(thread_id)
                .and_then(|thread| thread.members.get_mut(actor));
            let Some(position) = position else {
                continue;
            };
            if position.advance(kind, last.seq()) {
                read_to.push(last.clone());
            }
        }
        read_to
    }

    /// Rebuild a reply from a delivered range. A message that expired since is left out.
    fn render_delivered(&self, delivered: Delivered) -> InboxReply {
        let messages = delivered
            .ids
            .iter()
            .filter_map(|id| {
                let thread = self.kept_thread(id.thread())?;
                let message = self.kept_message(thread, id.seq())?;
                Some(thread.render(id.clone(), message, false))
            })
            .collect();
        delivered.reply(messages)
    }

    fn describe(&self, caller: &Caller, id: &ThreadId) -> Result<ThreadsReply, CommsError> {
        let thread = self.thread(id)?;
        rules::authorize_describe(id, &thread.access(caller))?;
        Ok(ThreadsReply {
            threads: vec![summarize(id, thread, &caller.actor, self.now_micros)],
            pending: Vec::new(),
            next_cursor: None,
            notices: Vec::new(),
        })
    }

    fn list(&self, caller: &Caller, filter: &ThreadFilter) -> Result<ThreadsReply, CommsError> {
        let selection = Selection::parse(filter)?;
        let matching = self
            .threads
            .iter()
            .filter(|(id, _)| self.kept_thread(id).is_some())
            .filter(|(_, thread)| selection.selects(&thread.access(caller), &thread.subject, &thread.paths))
            .map(|(id, thread)| (thread.last_activity_micros, id.clone(), thread))
            .collect();
        let page = rules::listing_page(
            matching,
            &selection.listing(caller),
            filter.cursor.as_ref(),
            filter.limit,
        )?;
        Ok(ThreadsReply {
            threads: page
                .rows
                .into_iter()
                .map(|(id, thread)| summarize(&id, thread, &caller.actor, self.now_micros))
                .collect(),
            pending: Vec::new(),
            next_cursor: page.next_cursor,
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
        let at_head = Position::at_head(thread.head);
        let change = rules::authorize_update(
            id,
            &update.action,
            &thread.access(caller),
            thread.members.len(),
            |agent| thread.members.contains_key(agent),
        )?;
        match change {
            Change::Nothing => {}
            Change::Join => {
                thread.members.insert(actor.clone(), at_head);
            }
            Change::Leave => {
                thread.members.remove(actor);
                if thread.members.is_empty() {
                    thread.archived = true;
                }
            }
            Change::Invite => {
                if let ThreadAction::Invite(invited) = &update.action {
                    for agent in invited {
                        thread.members.entry(agent.clone()).or_insert(at_head);
                    }
                }
            }
            Change::Archive => thread.archived = true,
        }
        Ok(pending)
    }
}

impl ThreadState {
    /// The facts the rules decide on, for `caller`.
    fn access(&self, caller: &Caller) -> Access {
        Access {
            member: self.members.contains_key(&caller.actor),
            open: self.open,
            same_repo: self.repo == caller.repo,
            archived: self.archived,
            creator: self.creator == caller.actor,
        }
    }

    fn render(&self, id: MessageId, message: &StoredMessage, whole: bool) -> InboxMessage {
        rules::render(
            id,
            &self.subject,
            &message.from,
            message.ts_micros,
            message.reply_to.clone(),
            &message.body,
            whole,
        )
    }
}

/// One thread as `actor` sees it at `now_micros`: the unread count is of what has not expired.
fn summarize(id: &ThreadId, thread: &ThreadState, actor: &Actor, now_micros: i64) -> ThreadSummary {
    let position = thread.members.get(actor);
    let unread = position.map_or(0, |position| {
        thread
            .messages
            .range(position.read_seq + 1..)
            .filter(|(_, message)| message.from != *actor && !rules::message_expired(message.ts_micros, now_micros))
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
