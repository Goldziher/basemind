//! Reading: one message by id, or a page of the caller's threads merged in post order.
//!
//! The threads a read covers are merged lazily: each is walked from where the caller stands in
//! it, one message row at a time and without bodies, only as far as the merge needs to know which
//! message comes next in the store-wide post order. A body is loaded only for a message the page
//! takes. Rows walked are bounded by one per covered thread plus [`SCAN_BUDGET`]; a read that
//! reaches the budget returns what it has with `more` set.
//!
//! What the budget mostly pays for is the caller's own posts, which no read returns. A read that
//! writes records how far it walked over nothing else (`Position::pass_own`), so the next read
//! starts past them; a peek continues past them with its cursor.
//!
//! A read over all the caller's threads leaves out the archived ones, but tells the caller about
//! each one the thread cap archived, with how many of its messages are unread
//! ([`rules::tells_cap_archive`]). A marking read records that it told, in the caller's marks.
//!
//! A read also says how many messages a sweep removed from a thread past where the caller
//! stood in it ([`rules::reports_swept`]). The first row a thread's walk finds gives that away,
//! so it costs no row of its own; the notice holds until a read takes the caller past the gap.

use ahash::AHashSet;

use super::records::{MessageMeta, Reader, ThreadRecord};
use super::{Txn, keys};
use crate::comms::v2::api::{
    Actor, Caller, CommsError, Cursor, InboxBody, InboxMessage, InboxReply, InboxSelect, MAX_LIMIT, MessageId,
    ReadKind, ThreadId, cap_archived_member_notice, swept_notice,
};
use crate::comms::v2::rules::{self, BodyForm, Delivered, DeliveredRange, Position};

/// Message rows one read may walk beyond the first of each thread it covers.
pub(super) const SCAN_BUDGET: usize = 4 * MAX_LIMIT as usize;

/// One inbox read. A read that moves a mark stages the caller's marks and, for a relayed
/// marking read, the ranges it delivered; a relayed read that delivered before returns those.
pub(super) fn read(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    body: &InboxBody,
    kind: ReadKind,
) -> Result<InboxReply, CommsError> {
    let actor = &caller.actor;
    if let InboxSelect::Message(id) = &body.select {
        return Ok(rules::inbox_reply(
            vec![lookup(reader, caller, id)?],
            Vec::new(),
            false,
            None,
        ));
    }
    let replay_key = caller
        .replay_key
        .as_ref()
        .filter(|_| kind == ReadKind::Marking)
        .map(|key| keys::replay_inbox(actor, key));
    if let Some(delivered) = reader.replay::<Delivered>(replay_key.as_deref())? {
        return redeliver(reader, actor, delivered);
    }
    let only = body.thread();
    let after = rules::cursor_position(kind, body.cursor.as_ref(), actor, only)?;
    let tells = rules::tells_cap_archive(kind, after.is_some());
    let Scope { covered, untold } = scope(reader, caller, only, tells)?;
    let mut notices = Vec::with_capacity(untold.len());
    for thread in &untold {
        let floor = thread.position.floor(ReadKind::Marking);
        let unread = reader.unread(&thread.id, floor, thread.record.head, actor)?;
        notices.push(cap_archived_member_notice(&thread.id, unread));
    }
    let mut merge = Merge::new(reader, &covered, kind, actor, after)?;
    let rendered = std::iter::from_fn(|| match merge.next() {
        Ok(Some((lane, seq, meta))) => {
            count_row();
            let thread = &covered[lane];
            let message = reader.render(&thread.id, &thread.record, seq, &meta, BodyForm::of(kind));
            Some(message.map(|message| (meta.order, message)))
        }
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    });
    let (page, cut) = rules::take_page(rendered, body.limit)?;
    let stopped_at = merge.stopped_at;
    let more = cut || stopped_at.is_some();
    let next_cursor = match (kind.paged(), cut, stopped_at) {
        (Some(_), true, _) => rules::page_cursor(kind, &page, true, actor, only),
        (Some(paged), false, Some(bound)) => {
            let last = page.last().map_or(0, |(order, _)| *order);
            Some(Cursor::for_inbox(actor, only, paged, bound.max(last)))
        }
        _ => None,
    };
    let messages: Vec<InboxMessage> = page.into_iter().map(|(_, message)| message).collect();
    for (thread, lane) in covered.iter().zip(&merge.lanes) {
        if let Some(swept) = lane.swept(thread.record.head) {
            notices.push(swept_notice(&thread.id, swept));
        }
    }
    let mut read_to = Vec::new();
    let mut ranges = Vec::new();
    if kind.writes() {
        let last_delivered = rules::last_delivered(&messages);
        let delivered: AHashSet<&MessageId> = messages.iter().map(|message| &message.id).collect();
        for (thread, lane) in covered.iter().zip(&merge.lanes) {
            let mut position = thread.position;
            if let Some(to) = last_delivered.get(&thread.id)
                && position.advance(kind, *to)
            {
                read_to.push(MessageId::new(thread.id.clone(), *to)?);
                ranges.push(DeliveredRange {
                    thread: thread.id.clone(),
                    after: thread.position.read_seq,
                    to: *to,
                });
            }
            let passed = match kind {
                ReadKind::Marking => lane.passed(&thread.id, &delivered)?,
                ReadKind::Announcing if thread.position.notified_seq <= thread.position.floor(ReadKind::Marking) => {
                    lane.passed(&thread.id, &AHashSet::new())?
                }
                _ => None,
            };
            if let Some(passed) = passed {
                position.pass_own(passed);
            }
            if !thread.stored || position != thread.position {
                txn.set_position(reader.ks, &thread.id, actor, position)?;
            }
        }
        for thread in &untold {
            let told = Position {
                told_cap_archive: true,
                ..thread.position
            };
            txn.set_position(reader.ks, &thread.id, actor, told)?;
        }
    }
    if let (Some(key), Some(delivered)) = (replay_key, Delivered::of(ranges, more, notices.clone())) {
        txn.remember(reader.ks, key, &delivered)?;
    }
    Ok(InboxReply {
        notices,
        ..rules::inbox_reply(messages, read_to, more, next_cursor)
    })
}

/// The rows of one covered thread a read walks, in sequence order.
type Rows = Box<dyn Iterator<Item = Result<(u64, MessageMeta), CommsError>>>;

/// One covered thread in the merge.
struct Lane {
    rows: Option<Rows>,
    /// The next message this read would return from the thread, not handed out yet.
    front: Option<(u64, MessageMeta)>,
    /// Post position of the last row walked: every message of the thread not walked yet comes
    /// after it. `None` before the first row.
    lower: Option<u64>,
    /// The rows walked, in order, and whether each is one the read returns.
    walked: Vec<(u64, bool)>,
    /// The sequence number the walk started at, when it started where the caller stands and
    /// the read reports what a sweep removed there.
    start: Option<u64>,
}

impl Lane {
    /// Whether the merge must walk this thread further before it knows what comes next.
    fn unresolved(&self) -> bool {
        self.front.is_none() && self.rows.is_some()
    }

    /// The last sequence number up to which every walked row is in `delivered` or is the
    /// caller's own post, when there is one.
    fn passed(&self, thread: &ThreadId, delivered: &AHashSet<&MessageId>) -> Result<Option<u64>, CommsError> {
        let mut passed = None;
        for (seq, candidate) in &self.walked {
            if *candidate && !delivered.contains(&MessageId::new(thread.clone(), *seq)?) {
                break;
            }
            passed = Some(*seq);
        }
        Ok(passed)
    }

    /// How many sequence numbers from where the walk started, up to `head`, hold no message
    /// because a sweep removed it: those below the first row walked, or all of them when the walk
    /// found none. `None` when there are none, the walk did not report them, or it was not
    /// walked.
    fn swept(&self, head: u64) -> Option<u64> {
        let start = self.start?;
        let first = match self.walked.first() {
            Some((seq, _)) => *seq,
            None if self.rows.is_none() => head + 1,
            None => return None,
        };
        Some(first - start).filter(|swept| *swept > 0)
    }
}

/// The covered threads merged in post order, walking no more rows than its budget.
struct Merge {
    lanes: Vec<Lane>,
    kind: ReadKind,
    actor: Actor,
    budget: usize,
    /// Set when the budget ran out: every message posted at or before this position that the read
    /// returns has been handed out.
    stopped_at: Option<u64>,
}

impl Merge {
    fn new(
        reader: &Reader<'_>,
        covered: &[Covered],
        kind: ReadKind,
        actor: &Actor,
        after: Option<u64>,
    ) -> Result<Self, CommsError> {
        let mut lanes = Vec::with_capacity(covered.len());
        for thread in covered {
            let (mut first, last) = (thread.position.floor(kind) + 1, thread.record.head);
            if let Some(after) = after.filter(|_| first <= last) {
                first = first_past(reader, &thread.id, first, last, after)?;
            }
            let rows: Option<Rows> = (first <= last).then(|| Box::new(reader.metas(&thread.id, first..=last)) as Rows);
            let start = (rows.is_some() && rules::reports_swept(kind, after.is_some())).then_some(first);
            lanes.push(Lane {
                rows,
                front: None,
                lower: None,
                walked: Vec::new(),
                start,
            });
        }
        Ok(Self {
            budget: covered.len() + SCAN_BUDGET,
            lanes,
            kind,
            actor: actor.clone(),
            stopped_at: None,
        })
    }

    /// The next message in post order, as `(lane, seq, row)`, or `None` at the end or the budget.
    fn next(&mut self) -> Result<Option<(usize, u64, MessageMeta)>, CommsError> {
        loop {
            let best = (0..self.lanes.len())
                .filter_map(|lane| self.lanes[lane].front.as_ref().map(|(_, meta)| (meta.order, lane)))
                .min();
            let blocker = (0..self.lanes.len())
                .filter(|lane| self.lanes[*lane].unresolved())
                .min_by_key(|lane| self.lanes[*lane].lower);
            match (best, blocker) {
                (Some((order, _)), Some(lane)) if self.lanes[lane].lower.is_none_or(|lower| lower < order) => {
                    self.walk(lane)?;
                }
                (None, Some(lane)) => self.walk(lane)?,
                (Some((_, lane)), _) => {
                    let (seq, meta) = self.lanes[lane].front.take().expect("the best lane has a front");
                    return Ok(Some((lane, seq, meta)));
                }
                (None, None) => return Ok(None),
            }
            if self.stopped_at.is_some() {
                return Ok(None);
            }
        }
    }

    /// Walk one row of `lane`, or stop the merge when the budget is spent.
    fn walk(&mut self, lane: usize) -> Result<(), CommsError> {
        if self.budget == 0 {
            self.stopped_at = self
                .lanes
                .iter()
                .filter(|lane| lane.unresolved())
                .filter_map(|lane| lane.lower)
                .min()
                .or(Some(0));
            return Ok(());
        }
        self.budget -= 1;
        count_row();
        let lane = &mut self.lanes[lane];
        let Some(rows) = lane.rows.as_mut() else {
            return Ok(());
        };
        match rows.next().transpose()? {
            None => lane.rows = None,
            Some((seq, meta)) => {
                let candidate = rules::is_candidate(self.kind, &meta.from, &self.actor);
                lane.lower = Some(meta.order);
                lane.walked.push((seq, candidate));
                if candidate {
                    lane.front = Some((seq, meta));
                }
            }
        }
        Ok(())
    }
}

/// A thread a read covers, with where the caller stands in it.
struct Covered {
    id: ThreadId,
    record: ThreadRecord,
    position: Position,
    /// Whether `position` is marks the caller has stored, or only where it starts.
    stored: bool,
}

/// What a read looks at, each list in id order.
#[derive(Default)]
struct Scope {
    /// The threads whose messages it returns: the one thread named, or every active thread the
    /// caller's principal is a member of.
    covered: Vec<Covered>,
    /// The threads the thread cap archived that a read over all threads tells the caller about.
    untold: Vec<Covered>,
}

/// The threads a read looks at. A read over all threads passes over an archived thread before its
/// marks are read, unless the thread cap archived it and the read `tells` of that.
fn scope(reader: &Reader<'_>, caller: &Caller, only: Option<&ThreadId>, tells: bool) -> Result<Scope, CommsError> {
    let actor = &caller.actor;
    let mut scope = Scope::default();
    if let Some(id) = only {
        let record = reader.thread(id)?;
        let position = match &record {
            Some(_) => reader.position(id, actor)?,
            None => None,
        };
        let access = record.as_ref().map(|record| record.access(caller, position.is_some()));
        rules::authorize_read(id, access.as_ref())?;
        if let (Some(record), Some((position, stored))) = (record, position) {
            scope.covered.push(Covered {
                id: id.clone(),
                record,
                position,
                stored,
            });
        }
        return Ok(scope);
    }
    for id in reader.member_threads(actor.principal())? {
        let Some(record) = reader.thread(&id)? else {
            continue;
        };
        if record.archived && !(record.archived_by_cap && tells) {
            continue;
        }
        let Some((position, stored)) = reader.position(&id, actor)? else {
            continue;
        };
        let thread = Covered {
            id,
            record,
            position,
            stored,
        };
        if !thread.record.archived {
            scope.covered.push(thread);
        } else if !position.told_cap_archive {
            scope.untold.push(thread);
        }
    }
    scope.covered.sort_by(|one, other| one.id.cmp(&other.id));
    scope.untold.sort_by(|one, other| one.id.cmp(&other.id));
    Ok(scope)
}

/// Rebuild a reply from the ranges a read delivered. A message swept since is left out.
fn redeliver(reader: &Reader<'_>, actor: &Actor, delivered: Delivered) -> Result<InboxReply, CommsError> {
    let mut messages = Vec::new();
    for range in &delivered.ranges {
        let Some(record) = reader.thread(&range.thread)? else {
            continue;
        };
        for entry in reader.metas(&range.thread, range.after + 1..=range.to) {
            let (seq, meta) = entry?;
            if rules::is_candidate(ReadKind::Marking, &meta.from, actor) {
                let message = reader.render(&range.thread, &record, seq, &meta, BodyForm::Inline)?;
                messages.push((meta.order, message));
            }
        }
    }
    delivered.reply(messages)
}

fn lookup(reader: &Reader<'_>, caller: &Caller, id: &MessageId) -> Result<InboxMessage, CommsError> {
    let record = reader.thread(id.thread())?;
    let (access, meta) = match &record {
        Some(record) => {
            let member = reader.member(id.thread(), &caller.actor.principal_id())?.is_some();
            (Some(record.access(caller, member)), reader.meta(id.thread(), id.seq())?)
        }
        None => (None, None),
    };
    rules::authorize_lookup(id, access.as_ref(), meta.is_some())?;
    match (record, meta) {
        (Some(record), Some(meta)) => reader.render(id.thread(), &record, id.seq(), &meta, BodyForm::Whole),
        _ => Err(CommsError::MessageNotFound { id: id.clone() }),
    }
}

/// The first sequence number in `first..=last` whose message was posted after post position
/// `after`, or `last + 1`. Post positions grow with sequence numbers, so this is a binary search.
///
/// A sequence number may have no message, since a sweep removed it. A probe therefore decides
/// by the next message that is stored, and cuts the search above itself when the range holds
/// none.
fn first_past(reader: &Reader<'_>, thread: &ThreadId, first: u64, last: u64, after: u64) -> Result<u64, CommsError> {
    let (mut low, mut high) = (first, last + 1);
    while low < high {
        let middle = low + (high - low) / 2;
        match reader.metas(thread, middle..=high - 1).next().transpose()? {
            Some((seq, meta)) if meta.order <= after => low = seq + 1,
            Some(_) | None => high = middle,
        }
    }
    Ok(low)
}

#[cfg(test)]
thread_local! {
    pub(super) static ROWS_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one message row or body read, for the tests that bound what a read costs.
fn count_row() {
    #[cfg(test)]
    ROWS_READ.with(|rows| rows.set(rows.get() + 1));
}
