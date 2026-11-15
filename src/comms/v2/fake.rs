//! An in-memory [`AgentOps`]: the executable reading of ADR-0021.
//!
//! It keeps no file, runs no daemon and takes its time from a clock the test moves, so every rule
//! (membership, read positions, authorization, replay, retention) can be asserted with exact
//! values. The store-backed implementation is checked against the same expectations.
//!
//! Only the inbox wait uses real time, in units a test can shrink with
//! [`FakeAgents::with_wait_unit`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::api::{
    Actor, AgentOps, COMMS_STORE_VER, CommsError, InboxMessage, InboxReply, InboxRequest, InboxSelect,
    MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, Mark, MessageId, PostReply, PostRequest,
    PostTarget, REPLAY_TTL, RESPONSE_BUDGET_BYTES, ReplayKey, RepoKey, ThreadAction, ThreadId, ThreadQuery,
    ThreadSummary, ThreadUpdate, ThreadsReply, WaitOutcome, check_body, checked_wait, effective_limit, inline_body,
    normalize_query, normalize_subject, normalize_thread_path, path_selects, shared_identity_notice, subject_matches,
};
use crate::path::RelPath;

/// Where the fake's clock starts, in microseconds since the unix epoch. Fixed, so timestamps in
/// assertions are exact.
pub const FAKE_EPOCH_MICROS: i64 = 1_700_000_000_000_000;

/// In-memory agent coordination with ADR-0021 semantics.
pub struct FakeAgents {
    state: Mutex<State>,
    arrived: Condvar,
    wait_unit: Duration,
}

struct State {
    now_micros: i64,
    posts: u64,
    waiting: usize,
    store_ver: u32,
    next_bits: u32,
    queued_bits: VecDeque<u32>,
    threads: BTreeMap<ThreadId, ThreadState>,
    seen: BTreeSet<Actor>,
    sessions: BTreeMap<String, usize>,
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

/// What an inbox read delivered, kept against its replay key.
#[derive(Clone)]
struct Delivered {
    ids: Vec<MessageId>,
    read_to: Vec<MessageId>,
    next_cursor: Option<String>,
    wait: WaitOutcome,
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
                waiting: 0,
                store_ver: COMMS_STORE_VER,
                next_bits: 0,
                queued_bits: VecDeque::new(),
                threads: BTreeMap::new(),
                seen: BTreeSet::new(),
                sessions: BTreeMap::new(),
                post_replays: BTreeMap::new(),
                inbox_replays: BTreeMap::new(),
            }),
            arrived: Condvar::new(),
            wait_unit: Duration::from_secs(1),
        }
    }

    /// Make one `wait_secs` unit last `unit` instead of a second, so a test of the timeout does not
    /// sleep for seconds.
    pub fn with_wait_unit(mut self, unit: Duration) -> Self {
        self.wait_unit = unit;
        self
    }

    /// Move the clock forward: the only way messages and replay keys age.
    pub fn advance(&self, by: Duration) {
        let mut state = self.lock();
        state.now_micros = state
            .now_micros
            .saturating_add(i64::try_from(by.as_micros()).unwrap_or(i64::MAX));
    }

    /// Draw the next thread ids from `bits` before falling back to the counter. Repeating a value
    /// reproduces an id collision.
    pub fn queue_thread_bits(&self, bits: impl IntoIterator<Item = u32>) {
        self.lock().queued_bits.extend(bits);
    }

    /// Set how many live `serve` sessions share `principal`. Above one, every reply to that
    /// principal or its sub-identities carries the shared-identity notice.
    pub fn set_live_sessions(&self, principal: &str, sessions: usize) {
        self.lock().sessions.insert(principal.to_string(), sessions);
    }

    /// Pretend the store was written at layout version `version`.
    pub fn set_store_version(&self, version: u32) {
        self.lock().store_ver = version;
    }

    /// `(read_seq, notified_seq)` of `actor` in `thread`; `None` when it is not a member.
    pub fn position(&self, thread: &ThreadId, actor: &Actor) -> Option<(u64, u64)> {
        let state = self.lock();
        let position = state.threads.get(thread)?.members.get(actor)?;
        Some((position.read_seq, position.notified_seq))
    }

    /// Number of inbox reads blocked in their wait right now, so a test can post only once its
    /// reader is waiting.
    pub fn waiting(&self) -> usize {
        self.lock().waiting
    }

    /// Number of threads the store holds, archived ones included.
    pub fn thread_count(&self) -> usize {
        self.lock().threads.len()
    }

    /// A panic in another test thread must not hide this one's assertion behind a poison error.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AgentOps for FakeAgents {
    fn post(&self, request: PostRequest) -> Result<PostReply, CommsError> {
        check_body(&request.body)?;
        let mut state = self.lock();
        state.begin(&request.actor);
        state.check_writable()?;
        let actor = request.actor.clone();
        let mut reply = state.post(request)?;
        reply.notice = state.notice(&actor);
        self.arrived.notify_all();
        Ok(reply)
    }

    fn inbox(&self, request: InboxRequest) -> Result<InboxReply, CommsError> {
        checked_wait(request.wait_secs)?;
        let deadline = Instant::now() + self.wait_unit * request.wait_secs;
        let mut state = self.lock();
        state.begin(&request.actor);
        if request.mark != Mark::Peek {
            state.check_writable()?;
        }
        let notice = state.notice(&request.actor);
        if let InboxSelect::Message(id) = &request.select {
            return Ok(InboxReply {
                messages: vec![state.lookup(&request.actor, id)?],
                read_to: Vec::new(),
                next_cursor: None,
                notice,
                wait: WaitOutcome::NotWaited,
            });
        }
        let replayed = request
            .replay_key
            .as_ref()
            .and_then(|key| state.replayed_inbox(&request.actor, key));
        if let Some(delivered) = replayed {
            return Ok(state.render_delivered(delivered, notice));
        }
        let after = parse_order_cursor(request.cursor.as_deref())?;
        let unread_only = request.unread || request.mark == Mark::Notify;
        let mut wait = WaitOutcome::NotWaited;
        let mut candidates = state.candidates(&request, unread_only, after)?;
        while candidates.is_empty() && unread_only {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if request.wait_secs > 0 {
                    wait = WaitOutcome::TimedOut;
                }
                break;
            }
            state.waiting += 1;
            state = self
                .arrived
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            state.waiting -= 1;
            state.expire();
            candidates = state.candidates(&request, unread_only, after)?;
            if !candidates.is_empty() {
                wait = WaitOutcome::Arrived;
            }
        }
        let delivered = state.deliver(&request, candidates, wait);
        if let Some(key) = request.replay_key {
            let expires = state.now_micros.saturating_add(ttl_micros(REPLAY_TTL));
            state
                .inbox_replays
                .insert((request.actor, key), (expires, delivered.clone()));
        }
        Ok(state.render_delivered(delivered, notice))
    }

    fn threads_query(&self, request: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        let mut state = self.lock();
        state.begin(&request.actor);
        let notice = state.notice(&request.actor);
        let mut reply = state.query(&request)?;
        reply.notice = notice;
        Ok(reply)
    }

    fn threads_update(&self, request: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        let mut state = self.lock();
        state.begin(&request.actor);
        state.check_writable()?;
        let pending = state.update(&request)?;
        let thread = state.thread(&request.thread)?;
        Ok(ThreadsReply {
            threads: vec![summarize(&request.thread, thread, &request.actor)],
            pending,
            next_cursor: None,
            notice: state.notice(&request.actor),
        })
    }
}

impl State {
    /// Every call counts as the caller having connected, and sees the store as retention left it.
    fn begin(&mut self, actor: &Actor) {
        self.seen.insert(actor.clone());
        self.expire();
    }

    fn expire(&mut self) {
        let now = self.now_micros;
        let cutoff = now.saturating_sub(ttl_micros(MESSAGE_TTL));
        for thread in self.threads.values_mut() {
            thread.messages.retain(|_, message| message.ts_micros > cutoff);
        }
        self.threads.retain(|_, thread| !thread.messages.is_empty());
        self.post_replays.retain(|_, (expires, _)| *expires > now);
        self.inbox_replays.retain(|_, (expires, _)| *expires > now);
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

    fn notice(&self, actor: &Actor) -> Option<String> {
        let sessions = self.sessions.get(actor.principal()).copied().unwrap_or(0);
        (sessions > 1).then(|| shared_identity_notice(actor.principal(), sessions))
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

    /// Recipients the store has never seen connect, the caller aside.
    fn pending(&self, actor: &Actor, recipients: &[Actor]) -> Vec<Actor> {
        let pending: BTreeSet<&Actor> = recipients
            .iter()
            .filter(|recipient| *recipient != actor && !self.seen.contains(*recipient))
            .collect();
        pending.into_iter().cloned().collect()
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

    fn post(&mut self, request: PostRequest) -> Result<PostReply, CommsError> {
        let PostRequest {
            actor,
            repo,
            target,
            body,
            replay_key,
        } = request;
        let (thread_id, reply_to, created, pending) = match target {
            PostTarget::NewThread { subject, to, paths } => {
                if let Some(reply) = self.replayed_post(&actor, None, replay_key.as_ref()) {
                    return Ok(reply);
                }
                let pending = self.pending(&actor, &to);
                let id = self.create_thread(&actor, repo, &subject, to, &paths)?;
                (id, None, true, pending)
            }
            PostTarget::Thread(id) => {
                self.thread(&id)?;
                (id, None, false, Vec::new())
            }
            PostTarget::ReplyTo(message) => {
                if !self.threads.contains_key(message.thread()) {
                    return Err(CommsError::MessageNotFound { id: message });
                }
                (message.thread().clone(), Some(message), false, Vec::new())
            }
        };
        if !created {
            if let Some(reply) = self.replayed_post(&actor, Some(&thread_id), replay_key.as_ref()) {
                return Ok(reply);
            }
            let thread = self.thread(&thread_id)?;
            if !thread.members.contains_key(&actor) {
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
            notice: None,
        };
        if let Some(key) = replay_key {
            let expires = now.saturating_add(ttl_micros(REPLAY_TTL));
            let scope = (!created).then_some(thread_id);
            self.post_replays.insert((actor, scope, key), (expires, reply.clone()));
        }
        Ok(reply)
    }

    fn replayed_post(&self, actor: &Actor, thread: Option<&ThreadId>, key: Option<&ReplayKey>) -> Option<PostReply> {
        let key = (actor.clone(), thread.cloned(), key?.clone());
        self.post_replays.get(&key).map(|(_, reply)| reply.clone())
    }

    fn create_thread(
        &mut self,
        creator: &Actor,
        repo: RepoKey,
        subject: &str,
        to: Vec<Actor>,
        paths: &[RelPath],
    ) -> Result<ThreadId, CommsError> {
        let subject = normalize_subject(subject)?;
        let mut paths = paths.iter().map(normalize_thread_path).collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        paths.dedup();
        let active = self
            .threads
            .values()
            .filter(|thread| thread.repo == repo && !thread.archived)
            .count();
        if active >= MAX_ACTIVE_THREADS_PER_REPO {
            return Err(CommsError::ThreadCapReached {
                repo,
                cap: MAX_ACTIVE_THREADS_PER_REPO,
            });
        }
        let start = Position {
            read_seq: 0,
            notified_seq: 0,
        };
        let open = to.is_empty();
        let members = to
            .into_iter()
            .chain([creator.clone()])
            .map(|member| (member, start))
            .collect();
        let id = self.fresh_thread_id();
        self.threads.insert(
            id.clone(),
            ThreadState {
                subject,
                repo,
                paths,
                open,
                archived: false,
                creator: creator.clone(),
                head: 0,
                last_activity_micros: self.now_micros,
                messages: BTreeMap::new(),
                members,
            },
        );
        Ok(id)
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

    fn replayed_inbox(&self, actor: &Actor, key: &ReplayKey) -> Option<Delivered> {
        self.inbox_replays
            .get(&(actor.clone(), key.clone()))
            .map(|(_, delivered)| delivered.clone())
    }

    /// The messages a read could return, in post order: `(order, id)`.
    fn candidates(
        &self,
        request: &InboxRequest,
        unread_only: bool,
        after: Option<u64>,
    ) -> Result<Vec<(u64, MessageId)>, CommsError> {
        let actor = &request.actor;
        let only = match &request.select {
            InboxSelect::Thread(only) => Some(only),
            InboxSelect::All | InboxSelect::Message(_) => None,
        };
        if let Some(only) = only {
            let thread = self.thread(only)?;
            if !thread.members.contains_key(actor) {
                return Err(CommsError::NotMember { thread: only.clone() });
            }
        }
        let mut candidates = Vec::new();
        for (id, thread) in &self.threads {
            let selected = only.is_none_or(|only| only == id);
            let Some(position) = thread.members.get(actor).filter(|_| selected) else {
                continue;
            };
            let floor = match (unread_only, request.mark) {
                (false, _) => 0,
                (true, Mark::Notify) => position.read_seq.max(position.notified_seq),
                (true, Mark::Read | Mark::Peek) => position.read_seq,
            };
            for (seq, message) in thread.messages.range(floor + 1..) {
                let own = unread_only && message.from == *actor;
                let paged_out = after.is_some_and(|after| message.order <= after);
                if !own && !paged_out {
                    candidates.push((message.order, MessageId::new(id.clone(), *seq)?));
                }
            }
        }
        candidates.sort();
        Ok(candidates)
    }

    /// Cut `candidates` to the limit and the response budget, then move the positions the
    /// request's [`Mark`] moves.
    fn deliver(&mut self, request: &InboxRequest, candidates: Vec<(u64, MessageId)>, wait: WaitOutcome) -> Delivered {
        let limit = effective_limit(request.limit);
        let total = candidates.len();
        let mut budget = RESPONSE_BUDGET_BYTES;
        let mut ids = Vec::new();
        let mut last_order = None;
        for (order, id) in candidates {
            let inline_len = self
                .threads
                .get(id.thread())
                .and_then(|thread| thread.messages.get(&id.seq()))
                .map_or(0, |message| inline_body(&message.body).0.len());
            if ids.len() == limit || (!ids.is_empty() && inline_len > budget) {
                break;
            }
            budget = budget.saturating_sub(inline_len);
            last_order = Some(order);
            ids.push(id);
        }
        let next_cursor = last_order.filter(|_| ids.len() < total).map(|order| order.to_string());
        let mut last_delivered: BTreeMap<&ThreadId, &MessageId> = BTreeMap::new();
        for id in &ids {
            let last = last_delivered.entry(id.thread()).or_insert(id);
            if id.seq() > last.seq() {
                *last = id;
            }
        }
        let mut read_to = Vec::new();
        for (thread_id, last) in last_delivered {
            let seq = last.seq();
            let position = self
                .threads
                .get_mut(thread_id)
                .and_then(|thread| thread.members.get_mut(&request.actor));
            let Some(position) = position else {
                continue;
            };
            match request.mark {
                Mark::Peek => {}
                Mark::Notify => position.notified_seq = position.notified_seq.max(seq),
                Mark::Read if seq > position.read_seq => {
                    position.read_seq = seq;
                    position.notified_seq = position.notified_seq.max(seq);
                    read_to.push(last.clone());
                }
                Mark::Read => {}
            }
        }
        Delivered {
            ids,
            read_to,
            next_cursor,
            wait,
        }
    }

    /// Rebuild a reply from the delivered range. A message that expired since is left out, as the
    /// store keeps the range and not a second copy of the bodies.
    fn render_delivered(&self, delivered: Delivered, notice: Option<String>) -> InboxReply {
        let messages = delivered
            .ids
            .into_iter()
            .filter_map(|id| {
                let thread = self.threads.get(id.thread())?;
                let message = thread.messages.get(&id.seq())?;
                Some(render(id, thread, message, false))
            })
            .collect();
        InboxReply {
            messages,
            read_to: delivered.read_to,
            next_cursor: delivered.next_cursor,
            notice,
            wait: delivered.wait,
        }
    }

    fn query(&self, request: &ThreadQuery) -> Result<ThreadsReply, CommsError> {
        let actor = &request.actor;
        let reply = |threads, next_cursor| ThreadsReply {
            threads,
            pending: Vec::new(),
            next_cursor,
            notice: None,
        };
        if let Some(id) = &request.thread {
            let thread = self.thread(id)?;
            if !thread.members.contains_key(actor) && !(thread.open && thread.repo == request.repo) {
                return Err(CommsError::ThreadClosed { thread: id.clone() });
            }
            return Ok(reply(vec![summarize(id, thread, actor)], None));
        }
        let query = request.query.as_deref().map(normalize_query).transpose()?;
        let path = request.path.as_ref().map(normalize_thread_path).transpose()?;
        let after = match request.cursor.as_deref() {
            Some(cursor) => Some(ThreadId::parse(cursor).map_err(|_| CommsError::InvalidCursor {
                cursor: cursor.to_string(),
            })?),
            None => None,
        };
        let discovering = query.is_some() || path.is_some();
        let limit = effective_limit(request.limit);
        let mut threads: Vec<ThreadSummary> = Vec::new();
        let mut next_cursor = None;
        for (id, thread) in &self.threads {
            let member = thread.members.contains_key(actor);
            let matches = !discovering
                || query
                    .as_deref()
                    .is_some_and(|query| subject_matches(&thread.subject, query))
                || path.as_ref().is_some_and(|path| path_selects(path, &thread.paths));
            let visible = member || (discovering && thread.open && thread.repo == request.repo);
            let paged_out = after.as_ref().is_some_and(|after| id <= after);
            if !visible || !matches || paged_out || (thread.archived && !request.archived) {
                continue;
            }
            if threads.len() == limit {
                next_cursor = threads.last().map(|last| last.id.to_string());
                break;
            }
            threads.push(summarize(id, thread, actor));
        }
        Ok(reply(threads, next_cursor))
    }

    /// Apply one membership or lifecycle change; returns the invited agents never seen.
    fn update(&mut self, request: &ThreadUpdate) -> Result<Vec<Actor>, CommsError> {
        let (actor, id) = (&request.actor, &request.thread);
        let pending = match &request.action {
            ThreadAction::Invite(invited) => self.pending(actor, invited),
            ThreadAction::Join | ThreadAction::Leave | ThreadAction::Archive => Vec::new(),
        };
        let thread = self.thread_mut(id)?;
        let member = thread.members.contains_key(actor);
        let at_head = Position {
            read_seq: thread.head,
            notified_seq: thread.head,
        };
        match &request.action {
            ThreadAction::Join if member => {}
            ThreadAction::Join if !thread.open || thread.repo != request.repo => {
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
            }
            ThreadAction::Invite(invited) if invited.is_empty() => return Err(CommsError::EmptyInvite),
            ThreadAction::Invite(_) if thread.archived => {
                return Err(CommsError::ThreadArchived { thread: id.clone() });
            }
            ThreadAction::Invite(invited) => {
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

fn parse_order_cursor(cursor: Option<&str>) -> Result<Option<u64>, CommsError> {
    cursor
        .map(|cursor| {
            cursor.parse::<u64>().map_err(|_| CommsError::InvalidCursor {
                cursor: cursor.to_string(),
            })
        })
        .transpose()
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

#[cfg(test)]
#[path = "fake_threads_tests.rs"]
mod threads_tests;
