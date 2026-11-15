//! Reading: one message by id, or a page of the caller's threads merged in post order.
//!
//! A page holds at most `limit` messages, so no thread contributes more than `limit + 1`
//! candidates. Each covered thread is walked that far from where the caller stands in it,
//! reading message rows without bodies; the candidates are sorted by post position and a body is
//! loaded only for a message the page takes.

use super::records::{MessageMeta, Reader, ThreadRecord};
use super::{Txn, keys};
use crate::comms::v2::api::{
    Actor, Caller, CommsError, InboxBody, InboxMessage, InboxReply, InboxSelect, MessageId, ReadKind, ThreadId,
    effective_limit,
};
use crate::comms::v2::rules::{self, BodyForm, Delivered, DeliveredRange, Position};

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
    let covered = covered(reader, caller, only)?;
    let per_thread = effective_limit(body.limit) + 1;
    let mut candidates: Vec<(u64, &Covered, u64, MessageMeta)> = Vec::new();
    for thread in &covered {
        let (mut first, last) = (thread.position.floor(kind) + 1, thread.record.head);
        if let Some(after) = after.filter(|_| first <= last) {
            first = first_past(reader, &thread.id, first, last, after)?;
        }
        if first > last {
            continue;
        }
        let mut found = 0;
        for entry in reader.metas(&thread.id, first..=last) {
            let (seq, meta) = entry?;
            if rules::is_candidate(kind, &meta.from, actor) {
                candidates.push((meta.order, thread, seq, meta));
                found += 1;
                if found == per_thread {
                    break;
                }
            }
        }
    }
    candidates.sort_by_key(|(order, ..)| *order);
    let rendered = candidates.into_iter().map(|(order, thread, seq, meta)| {
        let message = reader.render(&thread.id, &thread.record, seq, &meta, BodyForm::of(kind));
        message.map(|message| (order, message))
    });
    let (page, more) = rules::take_page(rendered, body.limit)?;
    let next_cursor = rules::page_cursor(kind, &page, more, actor, only);
    let messages: Vec<InboxMessage> = page.into_iter().map(|(_, message)| message).collect();
    let mut read_to = Vec::new();
    let mut ranges = Vec::new();
    if kind.writes() {
        let last_delivered = rules::last_delivered(&messages);
        for thread in &covered {
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
            if !thread.stored || position != thread.position {
                txn.set_position(reader.ks, &thread.id, actor, position)?;
            }
        }
    }
    if let (Some(key), Some(delivered)) = (replay_key, Delivered::of(ranges, more)) {
        txn.remember(reader.ks, key, &delivered)?;
    }
    Ok(rules::inbox_reply(messages, read_to, more, next_cursor))
}

/// A thread a read covers, with where the caller stands in it.
struct Covered {
    id: ThreadId,
    record: ThreadRecord,
    position: Position,
    /// Whether `position` is marks the caller has stored, or only where it starts.
    stored: bool,
}

/// The threads a read covers, in id order: the one thread named, or every active thread the
/// caller's principal is a member of.
fn covered(reader: &Reader<'_>, caller: &Caller, only: Option<&ThreadId>) -> Result<Vec<Covered>, CommsError> {
    let actor = &caller.actor;
    let ids = match only {
        Some(id) => vec![id.clone()],
        None => reader.member_threads(actor.principal())?,
    };
    let mut covered = Vec::with_capacity(ids.len());
    for id in ids {
        let record = reader.thread(&id)?;
        let position = match &record {
            Some(_) => reader.position(&id, actor)?,
            None => None,
        };
        if only.is_some() {
            let access = record.as_ref().map(|record| record.access(caller, position.is_some()));
            rules::authorize_read(&id, access.as_ref())?;
        }
        if let (Some(record), Some((position, stored))) = (record, position)
            && (only.is_some() || !record.archived)
        {
            covered.push(Covered {
                id,
                record,
                position,
                stored,
            });
        }
    }
    covered.sort_by(|one, other| one.id.cmp(&other.id));
    Ok(covered)
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
