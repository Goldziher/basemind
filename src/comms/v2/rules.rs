//! The decisions of ADR-0021 that do not depend on how a store keeps its data.
//!
//! Every [`AgentOps`](super::api::AgentOps) implementation answers the same questions: may this
//! caller post here, where does this read start, what does it move, which threads does a listing
//! show, has this record expired. They are answered once, here, as pure functions over facts the
//! implementation looks up, so the in-memory reference and a store-backed implementation cannot
//! drift apart on a rule. What stays with an implementation is only how it finds the facts and how
//! it writes the outcome.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::api::{
    Actor, COMMS_STORE_VER, Caller, CommsError, Cursor, InboxMessage, InboxReply, MAX_ACTIVE_THREADS_PER_REPO,
    MAX_RECIPIENTS, MAX_THREAD_MEMBERS, MESSAGE_TTL, MessageId, REPLAY_TTL, ReadKind, RepoKey, SEEN_REFRESH_INTERVAL,
    SEEN_TTL, ThreadAction, ThreadFilter, ThreadId, ThreadListing, WaitOutcome, check_recipient_count, deliverable,
    effective_limit, inline_body, message_cost, new_thread_recipients, normalize_query, normalize_subject,
    normalize_thread_path, normalize_thread_paths, path_selects, subject_matches,
};
use crate::path::RelPath;

/// `ttl` in microseconds, the unit every timestamp of the store is in.
pub fn ttl_micros(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX)
}

/// Refuse to write to a store whose layout version is newer than this binary's.
pub fn check_writable(store_ver: u32) -> Result<(), CommsError> {
    if store_ver > COMMS_STORE_VER {
        return Err(CommsError::StoreNewer {
            store: store_ver,
            binary: COMMS_STORE_VER,
        });
    }
    Ok(())
}

/// How far behind the previous stamp the clock may read, in microseconds, before a post is
/// stamped from the clock again and the difference is reported. Far above what a burst of posts
/// within one clock reading adds, which is one microsecond per post.
pub const CLOCK_BEHIND_TOLERANCE_MICROS: i64 = 60 * 1_000_000;

/// By how much `clock_micros` reads behind `last_micros`, the previous stamp, when that is more
/// than [`CLOCK_BEHIND_TOLERANCE_MICROS`]: the clock was ahead when that stamp was issued, or
/// has gone back since. A clock that is ahead cannot be told from a store nobody posted to for a
/// while, so only this direction is ever noticed.
pub fn clock_behind(clock_micros: i64, last_micros: i64) -> Option<i64> {
    let behind = last_micros.saturating_sub(clock_micros);
    (behind > CLOCK_BEHIND_TOLERANCE_MICROS).then_some(behind)
}

/// The timestamp of a post made when the clock reads `clock_micros` and the previous post was
/// stamped `last_micros`.
///
/// It is the clock's reading. While the clock is not later than the previous stamp, and no more
/// than [`CLOCK_BEHIND_TOLERANCE_MICROS`] behind it, it is one microsecond past that stamp
/// instead, so a clock that stands still or steps back a little does not stamp two posts alike.
/// Once the clock reads further behind ([`clock_behind`]) the previous stamp is taken to be the
/// wrong one and the clock is followed again: one post stamped while the clock ran ahead holds no
/// later post in the future. Timestamps therefore do not order posts; the store-wide post
/// position does.
pub fn post_timestamp(clock_micros: i64, last_micros: i64) -> i64 {
    if clock_behind(clock_micros, last_micros).is_some() {
        return clock_micros;
    }
    clock_micros.max(last_micros.saturating_add(1))
}

/// A thread's last activity once a post stamped `stamp_micros` is made to it, `previous_micros`
/// being what it was: the later of the two, so it never moves back.
///
/// A post can be stamped behind the ones before it, when the clock was set back or reads behind
/// for a while ([`post_timestamp`]). If the thread followed that stamp it would expire, with every
/// message of it, as soon as the clock is right again. The price is the other direction: a thread
/// posted to while the clock read ahead keeps that last activity until the clock passes it.
pub fn last_activity(previous_micros: i64, stamp_micros: i64) -> i64 {
    previous_micros.max(stamp_micros)
}

/// Whether a message posted at `ts_micros` is past [`MESSAGE_TTL`] at `now_micros`. The same
/// test on a thread's [`last_activity`] decides the thread, and a thread that has expired hides
/// every message of it. No message is stamped later than its thread's last activity, so none
/// outlives its thread; one stamped earlier expires before it, alone.
///
/// `now_micros` is the clock, never a timestamp the store issued. A message stamped ahead of
/// the clock is no exception: it expires when the clock passes its stamp by the retention,
/// which is later than it would have with a correct stamp, and so does its thread.
pub fn message_expired(ts_micros: i64, now_micros: i64) -> bool {
    ts_micros <= now_micros.saturating_sub(ttl_micros(MESSAGE_TTL))
}

/// Whether the seen record written at `recorded_micros` no longer makes its agent seen.
///
/// A record is rewritten only once it is [`SEEN_REFRESH_INTERVAL`] old (see
/// [`seen_record_is_current`]), so the agent's last call may be that much later than the record
/// says. The record is kept that much longer than [`SEEN_TTL`]: an agent is never reported
/// `pending` within [`SEEN_TTL`] of a call, and is reported at most one interval late.
pub fn seen_expired(recorded_micros: i64, now_micros: i64) -> bool {
    let kept = ttl_micros(SEEN_TTL).saturating_add(ttl_micros(SEEN_REFRESH_INTERVAL));
    recorded_micros <= now_micros.saturating_sub(kept)
}

/// Whether a successful call at `now_micros` may leave the caller's seen record, last written at
/// `recorded_micros`, as it is. It may not when there is none, which is what records an agent on
/// its first call, when the record is [`SEEN_REFRESH_INTERVAL`] old, or when it was written
/// ahead of the clock.
pub fn seen_record_is_current(recorded_micros: Option<i64>, now_micros: i64) -> bool {
    recorded_micros
        .is_some_and(|recorded| (0..ttl_micros(SEEN_REFRESH_INTERVAL)).contains(&now_micros.saturating_sub(recorded)))
}

/// When a replay entry written at `now_micros` stops answering for its key.
pub fn replay_expiry(now_micros: i64) -> i64 {
    now_micros.saturating_add(ttl_micros(REPLAY_TTL))
}

/// Whether a replay entry that expires at `expires_micros` no longer answers.
pub fn replay_expired(expires_micros: i64, now_micros: i64) -> bool {
    expires_micros <= now_micros
}

/// One row of the ADR's `members` table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Sequence number of the last message read.
    pub read_seq: u64,
    /// Sequence number of the last message the inbox hook announced.
    pub notified_seq: u64,
}

impl Position {
    /// Where the creator and every `to` recipient start: nothing read, nothing announced.
    pub const START: Self = Self {
        read_seq: 0,
        notified_seq: 0,
    };

    /// Where an agent that joins or is invited starts: what was said before is history.
    pub fn at_head(head: u64) -> Self {
        Self {
            read_seq: head,
            notified_seq: head,
        }
    }

    /// The sequence number a read of `kind` starts after.
    pub fn floor(self, kind: ReadKind) -> u64 {
        match kind {
            ReadKind::History | ReadKind::ById => 0,
            ReadKind::Announcing => self.read_seq.max(self.notified_seq),
            ReadKind::Marking | ReadKind::Peek => self.read_seq,
        }
    }

    /// Record that a read of `kind` delivered messages up to `seq` in this thread. Returns
    /// whether `read_seq` moved, which is what a reply reports in `read_to`. A read that marks
    /// moves `notified_seq` with it, so what was read is not announced afterwards.
    pub fn advance(&mut self, kind: ReadKind, seq: u64) -> bool {
        if !kind.writes() {
            return false;
        }
        self.notified_seq = self.notified_seq.max(seq);
        if kind == ReadKind::Marking && seq > self.read_seq {
            self.read_seq = seq;
            return true;
        }
        false
    }
}

/// What the rules need to know about a thread and the caller's relation to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// Whether the caller is a member.
    pub member: bool,
    /// Whether the thread is open to its repository.
    pub open: bool,
    /// Whether the thread belongs to the caller's repository.
    pub same_repo: bool,
    /// Whether the thread is archived.
    pub archived: bool,
    /// Whether the caller created the thread.
    pub creator: bool,
}

/// The `recipients` a reply may list under `pending`: without the caller and without duplicates,
/// in id order. The implementation keeps those it has not seen make a call.
pub fn addressed(actor: &Actor, recipients: &[Actor]) -> Vec<Actor> {
    let mut addressed: Vec<Actor> = recipients
        .iter()
        .filter(|recipient| *recipient != actor)
        .cloned()
        .collect();
    addressed.sort();
    addressed.dedup();
    addressed
}

/// A thread about to be created, with everything a caller chose checked and normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewThread {
    /// The subject as stored.
    pub subject: String,
    /// The paths as stored.
    pub paths: Vec<RelPath>,
    /// The members beside the creator.
    pub recipients: Vec<Actor>,
    /// Whether the thread is open to its repository: it is unless it was created with `to`.
    pub open: bool,
}

impl NewThread {
    /// Check a creating post: its recipients first, then its subject, then its paths, so the
    /// refusals come in one order everywhere.
    pub fn plan(creator: &Actor, subject: &str, to: &[Actor], paths: &[RelPath]) -> Result<Self, CommsError> {
        let recipients = new_thread_recipients(creator, to)?;
        let subject = normalize_subject(subject)?;
        let paths = normalize_thread_paths(paths)?;
        Ok(Self {
            subject,
            paths,
            open: recipients.is_empty(),
            recipients,
        })
    }
}

/// A new thread has its creator and at most [`MAX_RECIPIENTS`] others, so creating one can never
/// pass [`MAX_THREAD_MEMBERS`] and [`NewThread::plan`] needs no refusal for it.
const _: () = assert!(MAX_RECIPIENTS < MAX_THREAD_MEMBERS);

/// Refuse a change that would leave `thread`, which has `members` members, with `adding` more
/// than [`MAX_THREAD_MEMBERS`] allows.
fn check_member_cap(thread: &ThreadId, members: usize, adding: usize) -> Result<(), CommsError> {
    if adding > 0 && members.saturating_add(adding) > MAX_THREAD_MEMBERS {
        return Err(CommsError::MemberCapReached {
            thread: thread.clone(),
            cap: MAX_THREAD_MEMBERS,
        });
    }
    Ok(())
}

/// Refuse a new thread in a repository that already holds `active` active threads.
pub fn check_thread_cap(repo: &RepoKey, active: usize) -> Result<(), CommsError> {
    if active >= MAX_ACTIVE_THREADS_PER_REPO {
        return Err(CommsError::ThreadCapReached {
            repo: repo.clone(),
            cap: MAX_ACTIVE_THREADS_PER_REPO,
        });
    }
    Ok(())
}

/// Whether the caller may post into `thread`, directly or as a reply to `reply_to`.
///
/// `access` is `None` when the store does not hold the thread; `reply_target_stored` says whether
/// the message replied to is still stored, and is not consulted for a plain post.
pub fn authorize_post(
    thread: &ThreadId,
    access: Option<&Access>,
    reply_to: Option<&MessageId>,
    reply_target_stored: bool,
) -> Result<(), CommsError> {
    let access = match (access, reply_to) {
        (Some(access), _) => access,
        (None, Some(message)) => return Err(CommsError::MessageNotFound { id: message.clone() }),
        (None, None) => return Err(CommsError::ThreadNotFound { thread: thread.clone() }),
    };
    if !access.member {
        return Err(CommsError::NotMember { thread: thread.clone() });
    }
    if let Some(message) = reply_to.filter(|_| !reply_target_stored) {
        return Err(CommsError::MessageNotFound { id: message.clone() });
    }
    if access.archived {
        return Err(CommsError::ThreadArchived { thread: thread.clone() });
    }
    Ok(())
}

/// Whether the caller may read the messages of `thread`. `access` is `None` when the store does
/// not hold the thread.
pub fn authorize_read(thread: &ThreadId, access: Option<&Access>) -> Result<(), CommsError> {
    match access {
        None => Err(CommsError::ThreadNotFound { thread: thread.clone() }),
        Some(access) if !access.member => Err(CommsError::NotMember { thread: thread.clone() }),
        Some(_) => Ok(()),
    }
}

/// Whether the caller may read the message `id`. `access` is `None` when the store does not hold
/// its thread, and `stored` says whether it holds the message.
pub fn authorize_lookup(id: &MessageId, access: Option<&Access>, stored: bool) -> Result<(), CommsError> {
    let not_found = || CommsError::MessageNotFound { id: id.clone() };
    let access = access.ok_or_else(not_found)?;
    if !access.member {
        return Err(CommsError::NotMember {
            thread: id.thread().clone(),
        });
    }
    if !stored {
        return Err(not_found());
    }
    Ok(())
}

/// Whether the caller may have `thread` described: its members may, and so may any agent of its
/// repository while it is open.
pub fn authorize_describe(thread: &ThreadId, access: &Access) -> Result<(), CommsError> {
    if !access.member && !(access.open && access.same_repo) {
        return Err(CommsError::ThreadClosed { thread: thread.clone() });
    }
    Ok(())
}

/// What an authorized [`ThreadAction`] does to the thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Nothing: the caller joined a thread it is already in.
    Nothing,
    /// The caller becomes a member at the head.
    Join,
    /// The caller stops being a member; the thread is archived when nobody is left.
    Leave,
    /// Every invited agent that is not a member becomes one at the head.
    Invite,
    /// The thread is archived.
    Archive,
}

/// Whether the caller may apply `action` to `thread`, and what doing so changes.
///
/// `members` is how many members the thread has and `is_member` tells whether an agent is one:
/// a join, or an invitation of agents that are not members yet, is refused whole when it would
/// take the thread past [`MAX_THREAD_MEMBERS`]. Inviting only agents that are already members
/// adds nobody and is not refused for the cap.
pub fn authorize_update(
    thread: &ThreadId,
    action: &ThreadAction,
    access: &Access,
    members: usize,
    is_member: impl Fn(&Actor) -> bool,
) -> Result<Change, CommsError> {
    let id = thread;
    let thread = || id.clone();
    match action {
        ThreadAction::Join if access.member => Ok(Change::Nothing),
        ThreadAction::Join if !access.open || !access.same_repo => Err(CommsError::ThreadClosed { thread: thread() }),
        ThreadAction::Join if access.archived => Err(CommsError::ThreadArchived { thread: thread() }),
        ThreadAction::Join => {
            check_member_cap(id, members, 1)?;
            Ok(Change::Join)
        }
        ThreadAction::Leave | ThreadAction::Invite(_) if !access.member => {
            Err(CommsError::NotMember { thread: thread() })
        }
        ThreadAction::Leave => Ok(Change::Leave),
        ThreadAction::Invite(invited) if invited.is_empty() => Err(CommsError::EmptyInvite),
        ThreadAction::Invite(invited) => {
            check_recipient_count("invite", invited)?;
            if access.archived {
                return Err(CommsError::ThreadArchived { thread: thread() });
            }
            let mut adding: Vec<&Actor> = invited.iter().filter(|agent| !is_member(agent)).collect();
            adding.sort();
            adding.dedup();
            check_member_cap(id, members, adding.len())?;
            Ok(Change::Invite)
        }
        ThreadAction::Archive if !access.creator => Err(CommsError::NotCreator { thread: thread() }),
        ThreadAction::Archive => Ok(Change::Archive),
    }
}

/// Whether a stored message from `from` is one a read of `kind` by `actor` returns: history
/// returns everything, every other read only what other agents posted.
pub fn is_candidate(kind: ReadKind, from: &Actor, actor: &Actor) -> bool {
    kind == ReadKind::History || from != actor
}

/// The post position a read resumes after, when it pages with a cursor and was given one.
pub fn cursor_position(
    kind: ReadKind,
    cursor: Option<&Cursor>,
    actor: &Actor,
    thread: Option<&ThreadId>,
) -> Result<Option<u64>, CommsError> {
    match (kind.paged(), cursor) {
        (Some(read), Some(cursor)) => cursor.inbox_position(actor, thread, read).map(Some),
        _ => Ok(None),
    }
}

/// One message as an inbox reply carries it. `whole` is a read by id, which is the way to a body
/// longer than what is returned inline.
pub fn render(
    id: MessageId,
    subject: &str,
    from: &Actor,
    ts_micros: i64,
    reply_to: Option<MessageId>,
    body: &str,
    whole: bool,
) -> InboxMessage {
    let (body, truncated) = if whole { (body, false) } else { inline_body(body) };
    InboxMessage {
        id,
        subject: subject.to_string(),
        from: from.clone(),
        ts_micros,
        reply_to,
        body: body.to_string(),
        truncated,
    }
}

/// Cut one page from `candidates`, which come in store-wide post order as `(position, message)`.
///
/// Returns the messages delivered and whether any were left out. Candidates are pulled one at a
/// time and never past the first one that does not fit, so an implementation that produces them
/// lazily reads no more than one message beyond the page.
pub fn take_page<E>(
    mut candidates: impl Iterator<Item = Result<(u64, InboxMessage), E>>,
    limit: Option<u32>,
) -> Result<(Vec<(u64, InboxMessage)>, bool), E> {
    let mut pulled = Vec::new();
    let mut failure = None;
    let costs = std::iter::from_fn(|| match candidates.next()? {
        Ok(candidate) => {
            let cost = message_cost(&candidate.1);
            pulled.push(candidate);
            Some(cost)
        }
        Err(error) => {
            failure = Some(error);
            None
        }
    });
    let count = deliverable(costs, effective_limit(limit));
    if let Some(error) = failure {
        return Err(error);
    }
    let more = count < pulled.len();
    pulled.truncate(count);
    Ok((pulled, more))
}

/// The cursor a reply carries: only a read that pages with one, and only when messages were left
/// out, continuing after the last message of the `page`.
pub fn page_cursor(
    kind: ReadKind,
    page: &[(u64, InboxMessage)],
    more: bool,
    actor: &Actor,
    thread: Option<&ThreadId>,
) -> Option<Cursor> {
    match (kind.paged(), page.last()) {
        (Some(read), Some((order, _))) if more => Some(Cursor::for_inbox(actor, thread, read, *order)),
        _ => None,
    }
}

/// The last message of `delivered` in each thread: where a read that moves a position moves it
/// to. `delivered` is a prefix of the unread messages, so nothing unread is ever stepped over.
pub fn last_delivered(delivered: &[InboxMessage]) -> BTreeMap<&ThreadId, &MessageId> {
    let mut last_delivered: BTreeMap<&ThreadId, &MessageId> = BTreeMap::new();
    for message in delivered {
        let last = last_delivered.entry(message.id.thread()).or_insert(&message.id);
        if message.id.seq() > last.seq() {
            *last = &message.id;
        }
    }
    last_delivered
}

/// An inbox reply as a store returns it: no notices, and no wait, which is the daemon's.
pub fn inbox_reply(
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

/// What a read that moved a position delivered, kept against its replay key. The range and not
/// the bodies: a replay renders it again from the store, as stored, without a second
/// authorization. A read that delivered nothing keeps nothing, so repeating it reads afresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    /// The messages delivered, in the order they were returned.
    pub ids: Vec<MessageId>,
    /// The read positions the first delivery reported.
    pub read_to: Vec<MessageId>,
    /// Whether the first delivery left messages out.
    pub more: bool,
}

impl Delivered {
    /// What to keep for a read that delivered `messages`; `None` when it delivered nothing.
    pub fn of(messages: &[InboxMessage], read_to: &[MessageId], more: bool) -> Option<Self> {
        if messages.is_empty() {
            return None;
        }
        Some(Self {
            ids: messages.iter().map(|message| message.id.clone()).collect(),
            read_to: read_to.to_vec(),
            more,
        })
    }

    /// The reply of a replay, given the delivered messages that are still stored.
    pub fn reply(self, still_stored: Vec<InboxMessage>) -> InboxReply {
        inbox_reply(still_stored, self.read_to, self.more, None)
    }
}

/// A thread listing's filter, checked and in the form it is matched in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    query: Option<String>,
    path: Option<RelPath>,
    archived: bool,
}

impl Selection {
    /// Check `filter`'s query and path.
    pub fn parse(filter: &ThreadFilter) -> Result<Self, CommsError> {
        Ok(Self {
            query: filter.query.as_deref().map(normalize_query).transpose()?,
            path: filter.path.as_ref().map(normalize_thread_path).transpose()?,
            archived: filter.archived,
        })
    }

    /// What a cursor of this listing is bound to.
    pub fn listing<'a>(&'a self, caller: &'a Caller) -> ThreadListing<'a> {
        ThreadListing {
            actor: &caller.actor,
            repo: &caller.repo,
            query: self.query.as_deref(),
            path: self.path.as_ref(),
            archived: self.archived,
        }
    }

    /// Whether the listing reaches beyond the caller's own threads, to the open threads of its
    /// repository.
    pub fn discovers(&self) -> bool {
        self.query.is_some() || self.path.is_some()
    }

    /// Whether archived threads are listed.
    pub fn includes_archived(&self) -> bool {
        self.archived
    }

    /// Whether the listing shows a thread with this `subject` and these `paths` to a caller with
    /// this `access`.
    pub fn selects(&self, access: &Access, subject: &str, paths: &[RelPath]) -> bool {
        let visible = access.member || (self.discovers() && access.open && access.same_repo);
        let matches = self
            .query
            .as_deref()
            .is_none_or(|query| subject_matches(subject, query))
            && self.path.as_ref().is_none_or(|path| path_selects(path, paths));
        visible && matches && (self.archived || !access.archived)
    }
}

/// The order of a thread listing: last activity, newest first, then thread id.
pub fn listing_order(activity: i64, id: &ThreadId, other_activity: i64, other_id: &ThreadId) -> Ordering {
    other_activity.cmp(&activity).then_with(|| id.cmp(other_id))
}

/// One page of a thread listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingPage<T> {
    /// The page's threads in listing order, each with the payload it was given with.
    pub rows: Vec<(ThreadId, T)>,
    /// The cursor that continues the listing, when threads were left out.
    pub next_cursor: Option<Cursor>,
}

/// Cut one page from `matching`, which is every thread the listing selects, in any order, as
/// `(last activity, id, payload)`.
pub fn listing_page<T>(
    mut matching: Vec<(i64, ThreadId, T)>,
    listing: &ThreadListing<'_>,
    cursor: Option<&Cursor>,
    limit: Option<u32>,
) -> Result<ListingPage<T>, CommsError> {
    let after = cursor.map(|cursor| cursor.threads_position(listing)).transpose()?;
    matching.sort_by(|(activity, id, _), (other_activity, other_id, _)| {
        listing_order(*activity, id, *other_activity, other_id)
    });
    if let Some((after_activity, after_id)) = &after {
        matching.retain(|(activity, id, _)| activity < after_activity || (activity == after_activity && id > after_id));
    }
    let limit = effective_limit(limit);
    let next_cursor = match matching.get(limit.saturating_sub(1)) {
        Some((activity, id, _)) if matching.len() > limit => Some(Cursor::for_threads(listing, *activity, id)),
        _ => None,
    };
    let rows = matching.into_iter().take(limit).map(|(_, id, row)| (id, row)).collect();
    Ok(ListingPage { rows, next_cursor })
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
