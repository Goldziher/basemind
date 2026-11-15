//! The decisions of ADR-0021 that do not depend on how a store keeps its data.
//!
//! Every [`AgentOps`](super::api::AgentOps) implementation answers the same questions: may this
//! caller post here, where does this actor start reading, what does a read move, which threads
//! does a listing show, what does a sweep remove. They are answered once, here, as pure functions
//! over facts the implementation looks up, so the in-memory reference and a store-backed
//! implementation cannot drift apart on a rule. What stays with an implementation is only how it
//! finds the facts and how it writes the outcome.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::api::{
    Actor, COMMS_STORE_VER, Caller, CommsError, Cursor, InboxMessage, InboxReply, MAX_RECIPIENTS, MAX_THREAD_MEMBERS,
    MessageId, Principal, ReadKind, RepoKey, SEEN_REFRESH_INTERVAL, SEEN_TTL, ThreadAction, ThreadFilter, ThreadId,
    ThreadListing, WaitOutcome, check_recipient_count, deliverable, effective_limit, inline_body, message_cost,
    new_thread_recipients, normalize_query, normalize_subject, normalize_thread_path, normalize_thread_paths,
    path_selects, subject_matches,
};
use crate::path::RelPath;

/// How long a seen record is kept after it was written: [`SEEN_TTL`], plus the
/// [`SEEN_REFRESH_INTERVAL`] a record may trail its actor's last call by.
pub const SEEN_RETENTION: Duration = Duration::from_secs(SEEN_TTL.as_secs() + SEEN_REFRESH_INTERVAL.as_secs());

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

/// The timestamp of a post made when the clock reads `clock_micros` into a thread whose newest
/// message is stamped `thread_last_micros` (`None` for the post that creates the thread): the
/// clock, or one microsecond past the thread's last stamp when the clock is not later. Stamps
/// therefore rise with `seq` inside a thread and say nothing about order across threads.
pub fn post_timestamp(clock_micros: i64, thread_last_micros: Option<i64>) -> i64 {
    match thread_last_micros {
        Some(last) => clock_micros.max(last.saturating_add(1)),
        None => clock_micros,
    }
}

/// Whether a record stamped or written at `ts_micros` is more than `age` old at `now_micros`.
/// Only a sweep asks: no read hides a record for its age.
pub fn older_than(ts_micros: i64, now_micros: i64, age: Duration) -> bool {
    now_micros.saturating_sub(ts_micros) > ttl_micros(age)
}

/// Whether a successful call at `now_micros` writes the caller's seen record, last written at
/// `recorded_micros`: on its first call, and then once the record is [`SEEN_REFRESH_INTERVAL`] old.
pub fn seen_due(recorded_micros: Option<i64>, now_micros: i64) -> bool {
    recorded_micros.is_none_or(|recorded| now_micros.saturating_sub(recorded) >= ttl_micros(SEEN_REFRESH_INTERVAL))
}

/// An actor's two marks in one thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Sequence number of the last message read.
    pub read_seq: u64,
    /// Sequence number of the last message an announcing read told the actor about.
    pub notified_seq: u64,
}

impl Position {
    /// Where an actor without marks in a thread starts: at the highest `read_seq` any actor of
    /// its principal has reached there (`sibling_read_seq`), and with none at the principal's
    /// `member_start`, which is 0 for a member from the thread's creation and the head as of
    /// joining otherwise.
    pub fn start(sibling_read_seq: Option<u64>, member_start: u64) -> Self {
        let seq = sibling_read_seq.unwrap_or(member_start);
        Self {
            read_seq: seq,
            notified_seq: seq,
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

/// What the rules need to know about a thread and the caller's relation to it. Membership and
/// the creator's right are the principal's: a sub-identity has both through its principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// Whether the caller's principal is a member.
    pub member: bool,
    /// Whether the thread is open to its repository.
    pub open: bool,
    /// Whether the thread belongs to the caller's repository.
    pub same_repo: bool,
    /// Whether the thread is archived.
    pub archived: bool,
    /// Whether the caller's principal created the thread.
    pub creator: bool,
}

impl Access {
    /// The access of `caller` to a thread of `repo` created by `creator`.
    pub fn of(caller: &Caller, repo: &RepoKey, creator: &Actor, open: bool, archived: bool, member: bool) -> Self {
        Self {
            member,
            open,
            same_repo: *repo == caller.repo,
            archived,
            creator: creator.principal() == caller.actor.principal(),
        }
    }
}

/// The `recipients` a reply may list under `pending`: without the caller and without duplicates,
/// in id order. The implementation keeps those that have no seen record.
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

/// The principals of `actors`, without duplicates, in id order: who becomes a member when these
/// actors are named in `to` or `invite`.
pub fn principals<'a>(actors: impl IntoIterator<Item = &'a Actor>) -> Vec<Principal> {
    let mut principals: Vec<Principal> = actors.into_iter().map(Actor::principal_id).collect();
    principals.sort();
    principals.dedup();
    principals
}

/// A thread about to be created, with everything a caller chose checked and normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewThread {
    /// The subject as stored.
    pub subject: String,
    /// The paths as stored.
    pub paths: Vec<RelPath>,
    /// The actors named in `to`, the creator aside: whom the reply may list under `pending`.
    pub recipients: Vec<Actor>,
    /// The members from creation: the principals of the creator and of every recipient.
    pub members: Vec<Principal>,
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
            members: principals(recipients.iter().chain([creator])),
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
    if members.saturating_add(adding) > MAX_THREAD_MEMBERS {
        return Err(CommsError::MemberCapReached {
            thread: thread.clone(),
            cap: MAX_THREAD_MEMBERS,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Nothing: the change is already made. The caller joined a thread it is in, invited only
    /// members, or archived an archived thread; a replayed request ends here too.
    Nothing,
    /// The caller's principal becomes a member, starting at the head.
    Join,
    /// The caller's principal stops being a member and its actors lose their marks; the thread
    /// is archived when nobody is left.
    Leave,
    /// These principals, none of them a member yet, become members starting at the head.
    Invite(Vec<Principal>),
    /// The thread is archived.
    Archive,
}

/// Whether the caller may apply `action` to `thread`, and what doing so changes.
///
/// `members` is how many members the thread has and `is_member` tells whether a principal is
/// one: a join, or an invitation of principals that are not members yet, is refused whole when
/// it would take the thread past [`MAX_THREAD_MEMBERS`].
pub fn authorize_update(
    thread: &ThreadId,
    action: &ThreadAction,
    access: &Access,
    members: usize,
    is_member: impl Fn(&Principal) -> bool,
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
            let mut adding = principals(invited);
            adding.retain(|principal| !is_member(principal));
            if adding.is_empty() {
                return Ok(Change::Nothing);
            }
            if access.archived {
                return Err(CommsError::ThreadArchived { thread: thread() });
            }
            check_member_cap(id, members, adding.len())?;
            Ok(Change::Invite(adding))
        }
        ThreadAction::Archive if !access.creator => Err(CommsError::NotCreator { thread: thread() }),
        ThreadAction::Archive if access.archived => Ok(Change::Nothing),
        ThreadAction::Archive => Ok(Change::Archive),
    }
}

/// Whether a stored message from `from` is one a read of `kind` by `actor` returns: history
/// returns everything, every other read only what another actor posted. A sibling actor of the
/// same principal is another actor.
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

/// How much of a body a read of some kind returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyForm {
    /// Cut to what a listing returns.
    Inline,
    /// Whole: a read by id.
    Whole,
    /// None: an announcing read returns front matter only.
    FrontMatter,
}

impl BodyForm {
    /// The form a read of `kind` returns bodies in.
    pub fn of(kind: ReadKind) -> Self {
        match kind {
            ReadKind::ById => Self::Whole,
            ReadKind::Announcing => Self::FrontMatter,
            ReadKind::Marking | ReadKind::Peek | ReadKind::History => Self::Inline,
        }
    }
}

/// One message as an inbox reply carries it, its body in `form`.
pub fn render(
    id: MessageId,
    subject: &str,
    from: &Actor,
    ts_micros: i64,
    reply_to: Option<MessageId>,
    body: &str,
    form: BodyForm,
) -> InboxMessage {
    let (returned, truncated) = match form {
        BodyForm::Inline => inline_body(body),
        BodyForm::Whole => (body, false),
        BodyForm::FrontMatter => ("", false),
    };
    InboxMessage {
        id,
        subject: subject.to_string(),
        from: from.clone(),
        ts_micros,
        reply_to,
        body: returned.to_string(),
        body_bytes: body.len() as u64,
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

/// The highest sequence number of `delivered` in each thread: where a read that moves a mark
/// moves it to. `delivered` is a prefix of the unread messages, so nothing unread is stepped over.
pub fn last_delivered(delivered: &[InboxMessage]) -> BTreeMap<&ThreadId, u64> {
    let mut last_delivered: BTreeMap<&ThreadId, u64> = BTreeMap::new();
    for message in delivered {
        let last = last_delivered.entry(message.id.thread()).or_default();
        *last = (*last).max(message.id.seq());
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

/// The messages of one thread a marking read delivered: every message of another actor with a
/// sequence number above `after`, up to `to`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredRange {
    /// The thread.
    pub thread: ThreadId,
    /// The actor's `read_seq` before the read.
    pub after: u64,
    /// The actor's `read_seq` after it.
    pub to: u64,
}

/// What a marking read delivered, kept against its replay key: one range per thread and not the
/// bodies, so a replay renders them again from the store. A read that moved nothing keeps
/// nothing, so repeating it reads afresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    /// The ranges delivered, in thread order.
    pub ranges: Vec<DeliveredRange>,
    /// Whether the first delivery left messages out.
    pub more: bool,
}

impl Delivered {
    /// What to keep for a read that delivered `ranges`; `None` when it delivered nothing.
    pub fn of(ranges: Vec<DeliveredRange>, more: bool) -> Option<Self> {
        (!ranges.is_empty()).then_some(Self { ranges, more })
    }

    /// The reply of a replay, given the messages of the ranges that are still stored, as
    /// `(post position, message)` in any order.
    pub fn reply(self, mut still_stored: Vec<(u64, InboxMessage)>) -> Result<InboxReply, CommsError> {
        still_stored.sort_by_key(|(order, _)| *order);
        let messages = still_stored.into_iter().map(|(_, message)| message).collect();
        let read_to = self
            .ranges
            .into_iter()
            .map(|range| MessageId::new(range.thread, range.to))
            .collect::<Result<_, _>>()?;
        Ok(inbox_reply(messages, read_to, self.more, None))
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

/// One page of a thread listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingPage<T> {
    /// The page's rows in listing order.
    pub rows: Vec<T>,
    /// The cursor that continues the listing, when threads were left out.
    pub next_cursor: Option<Cursor>,
}

/// Cut one page from `matching`, which is every thread the listing selects, in any order, as
/// `(store-wide position of its newest post, payload)`. A listing is ordered by that position,
/// newest first; no two threads share one.
pub fn listing_page<T>(
    mut matching: Vec<(u64, T)>,
    listing: &ThreadListing<'_>,
    cursor: Option<&Cursor>,
    limit: Option<u32>,
) -> Result<ListingPage<T>, CommsError> {
    let after = cursor.map(|cursor| cursor.threads_position(listing)).transpose()?;
    matching.sort_by_key(|(order, _)| std::cmp::Reverse(*order));
    if let Some(after) = after {
        matching.retain(|(order, _)| *order < after);
    }
    let limit = effective_limit(limit);
    let next_cursor = match matching.get(limit.saturating_sub(1)) {
        Some((order, _)) if matching.len() > limit => Some(Cursor::for_threads(listing, *order)),
        _ => None,
    };
    let rows = matching.into_iter().take(limit).map(|(_, row)| row).collect();
    Ok(ListingPage { rows, next_cursor })
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
