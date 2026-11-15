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
    MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, MessageId, PostBody, PostReply, PostTarget,
    Principal, REPLAY_TTL, ReadKind, ReplayKey, RepoKey, ThreadAction, ThreadFilter, ThreadId, ThreadQuery,
    ThreadSummary, ThreadUpdate, ThreadsReply, cap_archive_notice, check_body,
};
use super::conformance::ConformanceHarness;
use super::rules::{self, Access, BodyForm, Change, Delivered, DeliveredRange, NewThread, Position, Selection};
use crate::path::RelPath;

/// Where the fake's clock starts, in microseconds since the unix epoch. Fixed, so timestamps in
/// the fake's own assertions are exact.
pub const FAKE_EPOCH_MICROS: i64 = 1_700_000_000_000_000;

/// In-memory agent coordination with ADR-0021 semantics.
pub struct FakeAgents {
    state: Mutex<State>,
}

struct State {
    /// What the clock reads: what posts are stamped from and what a sweep judges age against.
    now_micros: i64,
    /// Posts stored so far: the store-wide post order, and what [`AgentOps::arrivals`] reports.
    posts: u64,
    store_ver: u32,
    next_bits: u32,
    queued_bits: VecDeque<u32>,
    threads: BTreeMap<ThreadId, ThreadState>,
    /// When each actor's seen record was written.
    seen: BTreeMap<Actor, i64>,
    /// First replies of relayed posts, with when each was stored. A post into a thread is keyed
    /// with the thread, a creating post without.
    post_replays: BTreeMap<(Actor, Option<ThreadId>, ReplayKey), (i64, PostReply)>,
    inbox_replays: BTreeMap<(Actor, ReplayKey), (i64, Delivered)>,
    leave_replays: BTreeMap<(Actor, ThreadId, ReplayKey), (i64, ThreadsReply)>,
}

struct ThreadState {
    subject: String,
    repo: RepoKey,
    paths: Vec<RelPath>,
    open: bool,
    archived: bool,
    creator: Actor,
    head: u64,
    /// The newest message's timestamp: the thread's last activity.
    last_ts: i64,
    /// The newest message's position in the store-wide post order.
    last_order: u64,
    messages: BTreeMap<u64, StoredMessage>,
    /// Each member with where its actors start: 0 from creation, the head as of joining otherwise.
    members: BTreeMap<Principal, u64>,
    positions: BTreeMap<Actor, Position>,
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
                posts: 0,
                store_ver: COMMS_STORE_VER,
                next_bits: 0,
                queued_bits: VecDeque::new(),
                threads: BTreeMap::new(),
                seen: BTreeMap::new(),
                post_replays: BTreeMap::new(),
                inbox_replays: BTreeMap::new(),
                leave_replays: BTreeMap::new(),
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

    /// Remove what retention no longer keeps, judged against the clock as it reads now. Nothing
    /// else removes a record, and no read hides one. A store newer than the binary is not swept.
    pub fn sweep(&self) {
        let mut state = self.lock();
        if state.check_writable().is_ok() {
            state.sweep();
        }
    }

    /// Run one operation, writing the caller's seen record when it succeeds and the record is
    /// due. A store newer than the binary is not written to.
    fn call<T>(
        &self,
        caller: &Caller,
        operation: impl FnOnce(&mut State) -> Result<T, CommsError>,
    ) -> Result<T, CommsError> {
        let mut state = self.lock();
        let value = operation(&mut state)?;
        let now = state.now_micros;
        if state.check_writable().is_ok() && rules::seen_due(state.seen.get(&caller.actor).copied(), now) {
            state.seen.insert(caller.actor.clone(), now);
        }
        Ok(value)
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
        let position = self.lock().threads.get(thread)?.position(actor)?;
        Some((position.read_seq, position.notified_seq))
    }

    fn set_store_version(&self, version: u32) {
        self.lock().store_ver = version;
    }

    fn sweep(&self) {
        FakeAgents::sweep(self);
    }

    fn thread_count(&self) -> usize {
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
        self.call(caller, |state| state.update(caller, &update))
    }

    fn arrivals(&self) -> u64 {
        self.lock().posts
    }
}

fn threads_reply(threads: Vec<ThreadSummary>, pending: Vec<Actor>) -> ThreadsReply {
    ThreadsReply {
        threads,
        pending,
        next_cursor: None,
        notices: Vec::new(),
    }
}

impl State {
    /// One whole sweep: messages past their retention or outside the newest of their thread,
    /// threads idle past it with everything of theirs, replay entries past their hour, and seen
    /// records past theirs with the actor's marks in every thread.
    fn sweep(&mut self) {
        let now = self.now_micros;
        for thread in self.threads.values_mut() {
            let kept_from = thread.head.saturating_sub(MAX_MESSAGES_PER_THREAD);
            thread
                .messages
                .retain(|seq, message| *seq > kept_from && !rules::older_than(message.ts_micros, now, MESSAGE_TTL));
        }
        self.threads
            .retain(|_, thread| !rules::older_than(thread.last_ts, now, MESSAGE_TTL));
        self.post_replays
            .retain(|_, (written, _)| !rules::older_than(*written, now, REPLAY_TTL));
        self.inbox_replays
            .retain(|_, (written, _)| !rules::older_than(*written, now, REPLAY_TTL));
        self.leave_replays
            .retain(|_, (written, _)| !rules::older_than(*written, now, REPLAY_TTL));
        let threads = &mut self.threads;
        self.seen.retain(|actor, written| {
            let kept = !rules::older_than(*written, now, rules::SEEN_RETENTION);
            if !kept {
                for thread in threads.values_mut() {
                    thread.positions.remove(actor);
                }
            }
            kept
        });
    }

    fn check_writable(&self) -> Result<(), CommsError> {
        rules::check_writable(self.store_ver)
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

    /// The `recipients` without a seen record, the caller aside, in id order. Being named in a
    /// `to` or an `invite` does not make an actor seen.
    fn pending(&self, actor: &Actor, recipients: &[Actor]) -> Vec<Actor> {
        let mut pending = rules::addressed(actor, recipients);
        pending.retain(|recipient| !self.seen.contains_key(recipient));
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
        let (thread_id, reply_to, plan) = match target {
            PostTarget::NewThread { subject, to, paths } => {
                if let Some(reply) = self.replayed_post(actor, None, caller) {
                    return Ok(reply);
                }
                let plan = NewThread::plan(actor, &subject, &to, &paths)?;
                (self.fresh_thread_id(), None, Some(plan))
            }
            PostTarget::Thread(id) => (id, None, None),
            PostTarget::ReplyTo(message) => (message.thread().clone(), Some(message), None),
        };
        let mut reply = PostReply {
            id: MessageId::new(thread_id.clone(), 1)?,
            thread: thread_id.clone(),
            created: plan.is_some(),
            pending: Vec::new(),
            notices: Vec::new(),
        };
        match plan {
            Some(plan) => {
                reply.pending = self.pending(actor, &plan.recipients);
                reply.notices = self.archive_for_cap(&caller.repo).into_iter().collect();
                self.create_thread(caller, &thread_id, plan);
            }
            None => {
                if let Some(reply) = self.replayed_post(actor, Some(&thread_id), caller) {
                    return Ok(reply);
                }
                let thread = self.threads.get(&thread_id);
                let access = thread.map(|thread| thread.access(caller));
                let reply_target_stored = match (thread, &reply_to) {
                    (Some(thread), Some(message)) => thread.messages.contains_key(&message.seq()),
                    _ => true,
                };
                rules::authorize_post(&thread_id, access.as_ref(), reply_to.as_ref(), reply_target_stored)?;
            }
        }
        self.posts += 1;
        let (now, order) = (self.now_micros, self.posts);
        let thread = self.thread_mut(&thread_id)?;
        let ts_micros = rules::post_timestamp(now, (thread.head > 0).then_some(thread.last_ts));
        thread.head += 1;
        thread.last_ts = ts_micros;
        thread.last_order = order;
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
        reply.id = MessageId::new(thread_id.clone(), thread.head)?;
        if let Some(key) = &caller.replay_key {
            let scope = (!reply.created).then_some(thread_id);
            self.post_replays
                .insert((actor.clone(), scope, key.clone()), (now, reply.clone()));
        }
        Ok(reply)
    }

    fn replayed_post(&self, actor: &Actor, thread: Option<&ThreadId>, caller: &Caller) -> Option<PostReply> {
        let key = (actor.clone(), thread.cloned(), caller.replay_key.clone()?);
        self.post_replays.get(&key).map(|(_, reply)| reply.clone())
    }

    /// Make room for a new thread in `repo` when it is at its cap, by archiving the active
    /// thread whose newest post is the oldest. Returns the notice that says so.
    fn archive_for_cap(&mut self, repo: &RepoKey) -> Option<String> {
        let active = || {
            self.threads
                .iter()
                .filter(|(_, thread)| thread.repo == *repo && !thread.archived)
        };
        if active().count() < MAX_ACTIVE_THREADS_PER_REPO {
            return None;
        }
        let id = active().min_by_key(|(_, thread)| thread.last_order)?.0.clone();
        self.threads.get_mut(&id)?.archived = true;
        Some(cap_archive_notice(&id))
    }

    fn create_thread(&mut self, caller: &Caller, id: &ThreadId, plan: NewThread) {
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
                last_ts: 0,
                last_order: 0,
                messages: BTreeMap::new(),
                members: plan.members.into_iter().map(|member| (member, 0)).collect(),
                positions: BTreeMap::new(),
            },
        );
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
        if kind.writes() {
            self.check_writable()?;
        }
        let key = caller
            .replay_key
            .as_ref()
            .filter(|_| kind == ReadKind::Marking)
            .map(|key| (actor.clone(), key.clone()));
        if let Some((_, delivered)) = key.as_ref().and_then(|key| self.inbox_replays.get(key)) {
            return self.redeliver(actor, delivered.clone());
        }
        let after = rules::cursor_position(kind, body.cursor.as_ref(), actor, body.thread())?;
        let covered = self.covered(caller, body.thread())?;
        let mut candidates = Vec::new();
        for (id, position) in &covered {
            let thread = self.thread(id)?;
            for (seq, message) in thread.messages.range(position.floor(kind) + 1..) {
                let paged_out = after.is_some_and(|after| message.order <= after);
                if rules::is_candidate(kind, &message.from, actor) && !paged_out {
                    let rendered = thread.render(MessageId::new(id.clone(), *seq)?, message, BodyForm::of(kind));
                    candidates.push((message.order, rendered));
                }
            }
        }
        candidates.sort_by_key(|(order, _)| *order);
        let (page, more) = rules::take_page(candidates.into_iter().map(Ok::<_, CommsError>), body.limit)?;
        let next_cursor = rules::page_cursor(kind, &page, more, actor, body.thread());
        let messages: Vec<InboxMessage> = page.into_iter().map(|(_, message)| message).collect();
        let mut read_to = Vec::new();
        let mut ranges = Vec::new();
        if kind.writes() {
            let last_delivered = rules::last_delivered(&messages);
            for (id, before) in covered {
                let mut position = before;
                if let Some(to) = last_delivered.get(&id)
                    && position.advance(kind, *to)
                {
                    read_to.push(MessageId::new(id.clone(), *to)?);
                    ranges.push(DeliveredRange {
                        thread: id.clone(),
                        after: before.read_seq,
                        to: *to,
                    });
                }
                self.thread_mut(&id)?.positions.insert(actor.clone(), position);
            }
        }
        if let (Some(key), Some(delivered)) = (key, Delivered::of(ranges, more)) {
            self.inbox_replays.insert(key, (self.now_micros, delivered));
        }
        Ok(rules::inbox_reply(messages, read_to, more, next_cursor))
    }

    /// The threads a read covers, in id order, each with where the caller stands in it: the one
    /// thread named, or every active thread the caller's principal is a member of.
    fn covered(&self, caller: &Caller, only: Option<&ThreadId>) -> Result<Vec<(ThreadId, Position)>, CommsError> {
        if let Some(only) = only {
            let access = self.threads.get(only).map(|thread| thread.access(caller));
            rules::authorize_read(only, access.as_ref())?;
        }
        Ok(self
            .threads
            .iter()
            .filter(|(id, thread)| only.map_or(!thread.archived, |only| only == *id))
            .filter_map(|(id, thread)| Some((id.clone(), thread.position(&caller.actor)?)))
            .collect())
    }

    fn lookup(&self, caller: &Caller, id: &MessageId) -> Result<InboxMessage, CommsError> {
        let thread = self.threads.get(id.thread());
        let access = thread.map(|thread| thread.access(caller));
        let message = thread.and_then(|thread| Some((thread, thread.messages.get(&id.seq())?)));
        rules::authorize_lookup(id, access.as_ref(), message.is_some())?;
        let (thread, message) = message.ok_or_else(|| CommsError::MessageNotFound { id: id.clone() })?;
        Ok(thread.render(id.clone(), message, BodyForm::Whole))
    }

    /// Rebuild a reply from the ranges a read delivered. A message swept since is left out.
    fn redeliver(&self, actor: &Actor, delivered: Delivered) -> Result<InboxReply, CommsError> {
        let mut messages = Vec::new();
        for range in &delivered.ranges {
            let Some(thread) = self.threads.get(&range.thread) else {
                continue;
            };
            for (seq, message) in thread.messages.range(range.after + 1..=range.to) {
                if rules::is_candidate(ReadKind::Marking, &message.from, actor) {
                    let id = MessageId::new(range.thread.clone(), *seq)?;
                    messages.push((message.order, thread.render(id, message, BodyForm::Inline)));
                }
            }
        }
        delivered.reply(messages)
    }

    fn describe(&self, caller: &Caller, id: &ThreadId) -> Result<ThreadsReply, CommsError> {
        let thread = self.thread(id)?;
        rules::authorize_describe(id, &thread.access(caller))?;
        Ok(threads_reply(vec![thread.summarize(id, &caller.actor)], Vec::new()))
    }

    fn list(&self, caller: &Caller, filter: &ThreadFilter) -> Result<ThreadsReply, CommsError> {
        let selection = Selection::parse(filter)?;
        let matching = self
            .threads
            .iter()
            .filter(|(_, thread)| selection.selects(&thread.access(caller), &thread.subject, &thread.paths))
            .map(|(id, thread)| (thread.last_order, thread.summarize(id, &caller.actor)))
            .collect();
        let page = rules::listing_page(
            matching,
            &selection.listing(caller),
            filter.cursor.as_ref(),
            filter.limit,
        )?;
        Ok(ThreadsReply {
            next_cursor: page.next_cursor,
            ..threads_reply(page.rows, Vec::new())
        })
    }

    /// Apply one membership or lifecycle change and describe the thread as changed.
    fn update(&mut self, caller: &Caller, update: &ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        self.check_writable()?;
        let (actor, id) = (&caller.actor, &update.thread);
        let leave_key = caller
            .replay_key
            .as_ref()
            .filter(|_| update.action == ThreadAction::Leave)
            .map(|key| (actor.clone(), id.clone(), key.clone()));
        if let Some((_, reply)) = leave_key.as_ref().and_then(|key| self.leave_replays.get(key)) {
            return Ok(reply.clone());
        }
        let pending = match &update.action {
            ThreadAction::Invite(invited) => self.pending(actor, invited),
            ThreadAction::Join | ThreadAction::Leave | ThreadAction::Archive => Vec::new(),
        };
        let now = self.now_micros;
        let thread = self.thread_mut(id)?;
        let change = rules::authorize_update(
            id,
            &update.action,
            &thread.access(caller),
            thread.members.len(),
            |principal| thread.members.contains_key(principal),
        )?;
        match change {
            Change::Nothing => {}
            Change::Join => {
                thread.members.insert(actor.principal_id(), thread.head);
            }
            Change::Leave => {
                thread.members.remove(&actor.principal_id());
                thread
                    .positions
                    .retain(|sibling, _| sibling.principal() != actor.principal());
                thread.archived |= thread.members.is_empty();
            }
            Change::Invite(invited) => {
                thread
                    .members
                    .extend(invited.into_iter().map(|member| (member, thread.head)));
            }
            Change::Archive => thread.archived = true,
        }
        let reply = threads_reply(vec![thread.summarize(id, actor)], pending);
        if let Some(key) = leave_key {
            self.leave_replays.insert(key, (now, reply.clone()));
        }
        Ok(reply)
    }
}

impl ThreadState {
    /// The facts the rules decide on, for `caller`.
    fn access(&self, caller: &Caller) -> Access {
        let member = self.members.contains_key(&caller.actor.principal_id());
        Access::of(caller, &self.repo, &self.creator, self.open, self.archived, member)
    }

    /// Where `actor` stands: its marks, or where it starts when it has none. `None` when its
    /// principal is not a member.
    fn position(&self, actor: &Actor) -> Option<Position> {
        let member_start = *self.members.get(&actor.principal_id())?;
        let sibling_read_seq = || {
            self.positions
                .iter()
                .filter(|(sibling, _)| sibling.principal() == actor.principal())
                .map(|(_, position)| position.read_seq)
                .max()
        };
        Some(
            self.positions
                .get(actor)
                .copied()
                .unwrap_or_else(|| Position::start(sibling_read_seq(), member_start)),
        )
    }

    fn render(&self, id: MessageId, message: &StoredMessage, form: BodyForm) -> InboxMessage {
        rules::render(
            id,
            &self.subject,
            &message.from,
            message.ts_micros,
            message.reply_to.clone(),
            &message.body,
            form,
        )
    }

    /// The thread as `actor` sees it.
    fn summarize(&self, id: &ThreadId, actor: &Actor) -> ThreadSummary {
        let position = self.position(actor);
        let unread = position.map_or(0, |position| {
            self.messages
                .range(position.read_seq + 1..)
                .filter(|(_, message)| message.from != *actor)
                .count() as u64
        });
        ThreadSummary {
            id: id.clone(),
            subject: self.subject.clone(),
            repo: self.repo.clone(),
            paths: self.paths.clone(),
            open: self.open,
            archived: self.archived,
            creator: self.creator.clone(),
            member: position.is_some(),
            unread,
            head: self.head,
            last_activity_micros: self.last_ts,
            peers: self
                .members
                .keys()
                .filter(|member| member.as_str() != actor.principal())
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod tests;
