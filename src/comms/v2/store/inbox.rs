//! Reading: one message by id, or a page of the caller's threads merged in post order.
//!
//! A page costs what it returns plus one step per thread the caller is in. A thread whose newest
//! message from somebody else is not past the caller's position is dismissed from its `threads`
//! row alone; the others are walked from that position, each through its own range of `messages`,
//! and merged by post position, one message at a time, until the page is full.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::records::{MessageMeta, Reader, ThreadRecord};
use super::{Txn, keys};
use crate::comms::v2::api::{
    Actor, Caller, CommsError, InboxBody, InboxMessage, InboxReply, InboxSelect, MessageId, ReadKind, ThreadId,
};
use crate::comms::v2::rules::{self, Delivered, Position};

/// A read that moves nothing: by id, a peek, or history.
pub(super) fn read(
    reader: &Reader<'_>,
    caller: &Caller,
    body: &InboxBody,
    kind: ReadKind,
) -> Result<InboxReply, CommsError> {
    if let InboxSelect::Message(id) = &body.select {
        return Ok(rules::inbox_reply(
            vec![lookup(reader, caller, id)?],
            Vec::new(),
            false,
            None,
        ));
    }
    let (page, more) = page(reader, caller, body, kind)?;
    let next_cursor = rules::page_cursor(kind, &page, more, &caller.actor, body.thread());
    let messages = page.into_iter().map(|(_, message)| message).collect();
    Ok(rules::inbox_reply(messages, Vec::new(), more, next_cursor))
}

/// A read that moves a position: it stages the caller's new positions and, for a relayed
/// request, the range it delivered. A relayed request that delivered before returns that range.
pub(super) fn read_and_mark(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    body: &InboxBody,
    kind: ReadKind,
) -> Result<InboxReply, CommsError> {
    let actor = &caller.actor;
    let replay_key = caller.replay_key.as_ref().map(|key| keys::replay_inbox(actor, key));
    if let Some(key) = &replay_key
        && let Some(delivered) = reader.replay::<Delivered>(key)?
    {
        return redeliver(reader, delivered);
    }
    let (page, more) = page(reader, caller, body, kind)?;
    let messages: Vec<InboxMessage> = page.into_iter().map(|(_, message)| message).collect();
    let mut read_to = Vec::new();
    for (thread, last) in rules::last_delivered(&messages) {
        let Some(before) = reader.position(thread, actor)? else {
            continue;
        };
        let mut position = before;
        if position.advance(kind, last.seq()) {
            read_to.push(last.clone());
        }
        if position != before {
            txn.set_position(reader.ks, thread, actor, position)?;
        }
    }
    if let (Some(key), Some(delivered)) = (replay_key, Delivered::of(&messages, &read_to, more)) {
        txn.remember(reader, key, &delivered)?;
    }
    Ok(rules::inbox_reply(messages, read_to, more, None))
}

/// Rebuild a reply from a delivered range. A message that expired since is left out.
fn redeliver(reader: &Reader<'_>, delivered: Delivered) -> Result<InboxReply, CommsError> {
    let mut messages = Vec::with_capacity(delivered.ids.len());
    for id in &delivered.ids {
        let Some(record) = reader.thread(id.thread())? else {
            continue;
        };
        if let Some(meta) = reader.meta(id.thread(), id.seq())? {
            messages.push(reader.render(id.thread(), &record, id.seq(), &meta, false)?);
        }
    }
    Ok(delivered.reply(messages))
}

fn lookup(reader: &Reader<'_>, caller: &Caller, id: &MessageId) -> Result<InboxMessage, CommsError> {
    let record = reader.thread(id.thread())?;
    let (access, meta) = match &record {
        Some(record) => {
            let member = reader.position(id.thread(), &caller.actor)?.is_some();
            (Some(record.access(caller, member)), reader.meta(id.thread(), id.seq())?)
        }
        None => (None, None),
    };
    rules::authorize_lookup(id, access.as_ref(), meta.is_some())?;
    match (record, meta) {
        (Some(record), Some(meta)) => reader.render(id.thread(), &record, id.seq(), &meta, true),
        _ => Err(CommsError::MessageNotFound { id: id.clone() }),
    }
}

/// One page of the messages a read of `kind` returns, with their post positions, and whether
/// any were left out.
fn page(
    reader: &Reader<'_>,
    caller: &Caller,
    body: &InboxBody,
    kind: ReadKind,
) -> Result<(Vec<(u64, InboxMessage)>, bool), CommsError> {
    let actor = &caller.actor;
    let only = body.thread();
    let after = rules::cursor_position(kind, body.cursor.as_ref(), actor, only)?;
    let mut scans = Vec::new();
    let mut open = |id: &ThreadId, record: ThreadRecord, position: Position| -> Result<(), CommsError> {
        if let Some(scan) = Scan::open(reader, id, record, position, kind, actor, after)? {
            scans.push(scan);
        }
        Ok(())
    };
    match only {
        Some(id) => {
            let record = reader.thread(id)?;
            let position = match &record {
                Some(_) => reader.position(id, actor)?,
                None => None,
            };
            let access = record.as_ref().map(|record| record.access(caller, position.is_some()));
            rules::authorize_read(id, access.as_ref())?;
            if let (Some(record), Some(position)) = (record, position) {
                open(id, record, position)?;
            }
        }
        None => {
            for id in reader.member_threads(actor)? {
                let Some(record) = reader.thread(&id)? else {
                    continue;
                };
                if let Some(position) = reader.position(&id, actor)? {
                    open(&id, record, position)?;
                }
            }
        }
    }
    rules::take_page(Merged::new(reader, scans, kind, actor), body.limit)
}

/// The candidates of one thread, oldest first, with the next one read ahead.
struct Scan {
    thread: ThreadId,
    record: ThreadRecord,
    metas: Box<dyn Iterator<Item = Result<(u64, MessageMeta), CommsError>>>,
    next: Option<(u64, MessageMeta)>,
}

impl Scan {
    /// Start reading `id` for `actor`, or `None` when the thread cannot hold a candidate: its
    /// newest message that could be one is not past where the read starts.
    fn open(
        reader: &Reader<'_>,
        id: &ThreadId,
        record: ThreadRecord,
        position: Position,
        kind: ReadKind,
        actor: &Actor,
        after: Option<u64>,
    ) -> Result<Option<Self>, CommsError> {
        let last = if kind == ReadKind::History {
            record.head
        } else {
            record.newest_from_others(actor)
        };
        let mut first = record.first_stored(position.floor(kind) + 1);
        if let Some(after) = after.filter(|_| first <= last) {
            first = first_past(reader, id, first, last, after)?;
        }
        if first > last {
            return Ok(None);
        }
        let mut scan = Scan {
            thread: id.clone(),
            record,
            metas: Box::new(reader.metas(id, first..=last)),
            next: None,
        };
        scan.advance(kind, actor)?;
        Ok(scan.next.is_some().then_some(scan))
    }

    /// Read ahead to the next message the read returns.
    fn advance(&mut self, kind: ReadKind, actor: &Actor) -> Result<(), CommsError> {
        self.next = None;
        for entry in self.metas.by_ref() {
            let (seq, meta) = entry?;
            if rules::is_candidate(kind, &meta.from, actor) {
                self.next = Some((seq, meta));
                break;
            }
        }
        Ok(())
    }
}

/// The first sequence number in `first..=last` whose message was posted after post position
/// `after`, or `last + 1`. Post positions grow with sequence numbers, so this is a binary search.
///
/// A sequence number may have no message: the thread's cap or a sweep removed it, and a sweep
/// removes by timestamp, which need not be oldest first. A probe that finds none therefore
/// decides by the next message that is stored, and cuts the search above itself when the range
/// holds none.
fn first_past(reader: &Reader<'_>, thread: &ThreadId, first: u64, last: u64, after: u64) -> Result<u64, CommsError> {
    let (mut low, mut high) = (first, last + 1);
    while low < high {
        let middle = low + (high - low) / 2;
        match reader.stored_meta_from(thread, middle..=high - 1)? {
            Some((seq, meta)) if meta.order <= after => low = seq + 1,
            Some(_) | None => high = middle,
        }
    }
    Ok(low)
}

/// The candidates of several threads merged into store-wide post order, each rendered only when
/// it is pulled.
struct Merged<'r, 'k> {
    reader: &'r Reader<'k>,
    scans: Vec<Scan>,
    /// Post position of each scan's read-ahead message, smallest first.
    heads: BinaryHeap<Reverse<(u64, usize)>>,
    kind: ReadKind,
    actor: &'r Actor,
}

impl<'r, 'k> Merged<'r, 'k> {
    fn new(reader: &'r Reader<'k>, scans: Vec<Scan>, kind: ReadKind, actor: &'r Actor) -> Self {
        let heads = scans
            .iter()
            .enumerate()
            .filter_map(|(index, scan)| Some(Reverse((scan.next.as_ref()?.1.order, index))))
            .collect();
        Self {
            reader,
            scans,
            heads,
            kind,
            actor,
        }
    }

    fn pull(&mut self) -> Result<Option<(u64, InboxMessage)>, CommsError> {
        let Some(Reverse((order, index))) = self.heads.pop() else {
            return Ok(None);
        };
        let scan = &mut self.scans[index];
        let Some((seq, meta)) = scan.next.take() else {
            return Ok(None);
        };
        let message = self.reader.render(&scan.thread, &scan.record, seq, &meta, false)?;
        scan.advance(self.kind, self.actor)?;
        if let Some((_, next)) = &scan.next {
            self.heads.push(Reverse((next.order, index)));
        }
        Ok(Some((order, message)))
    }
}

impl Iterator for Merged<'_, '_> {
    type Item = Result<(u64, InboxMessage), CommsError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.pull().transpose()
    }
}
