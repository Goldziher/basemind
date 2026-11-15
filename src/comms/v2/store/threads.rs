//! Threads: describing one, listing and discovering them, and the four changes to one.

use ahash::AHashSet;

use super::records::{Reader, ThreadRecord};
use super::{Txn, keys};
use crate::comms::v2::api::{
    Actor, Caller, CommsError, Principal, ThreadAction, ThreadFilter, ThreadId, ThreadSummary, ThreadUpdate,
    ThreadsReply,
};
use crate::comms::v2::rules::{self, Change, Selection};

fn not_found(id: &ThreadId) -> CommsError {
    CommsError::ThreadNotFound { thread: id.clone() }
}

fn reply(threads: Vec<ThreadSummary>, pending: Vec<Actor>) -> ThreadsReply {
    ThreadsReply {
        threads,
        pending,
        next_cursor: None,
        notices: Vec::new(),
    }
}

/// Describe one thread to a caller the rules allow to see it.
pub(super) fn describe(reader: &Reader<'_>, caller: &Caller, id: &ThreadId) -> Result<ThreadsReply, CommsError> {
    let record = reader.thread(id)?.ok_or_else(|| not_found(id))?;
    let members = reader.members(id)?;
    let member = members.contains(&caller.actor.principal_id());
    rules::authorize_describe(id, &record.access(caller, member))?;
    Ok(reply(
        vec![summarize(reader, id, &record, &caller.actor, members)?],
        Vec::new(),
    ))
}

/// List the caller's threads and, when the filter discovers, the open threads of its repository.
///
/// The candidates come from two indexes, `member_threads` for the caller's principal and
/// `repo_threads` for its repository, so the cost is the threads the caller is in plus those of
/// the repository, and only the threads of the returned page are summarised.
pub(super) fn list(reader: &Reader<'_>, caller: &Caller, filter: &ThreadFilter) -> Result<ThreadsReply, CommsError> {
    let actor = &caller.actor;
    let selection = Selection::parse(filter)?;
    let mut candidates: Vec<(ThreadId, bool)> = Vec::new();
    let mut own = AHashSet::new();
    for id in reader.member_threads(actor.principal())? {
        own.insert(id.clone());
        candidates.push((id, true));
    }
    if selection.discovers() {
        let states: &[bool] = if selection.includes_archived() {
            &[false, true]
        } else {
            &[false]
        };
        for archived in states {
            let discovered = reader.repo_threads(&caller.repo, *archived)?;
            candidates.extend(
                discovered
                    .into_iter()
                    .filter(|id| !own.contains(id))
                    .map(|id| (id, false)),
            );
        }
    }
    let mut matching = Vec::new();
    for (id, member) in candidates {
        let Some(record) = reader.thread(&id)? else {
            continue;
        };
        if selection.selects(&record.access(caller, member), &record.subject, &record.paths) {
            matching.push((record.last_order, (id, record)));
        }
    }
    let page = rules::listing_page(
        matching,
        &selection.listing(caller),
        filter.cursor.as_ref(),
        filter.limit,
    )?;
    let mut threads = Vec::with_capacity(page.rows.len());
    for (id, record) in page.rows {
        threads.push(summarize(reader, &id, &record, actor, reader.members(&id)?)?);
    }
    Ok(ThreadsReply {
        next_cursor: page.next_cursor,
        ..reply(threads, Vec::new())
    })
}

/// Stage one membership or lifecycle change and describe the thread as changed. A relayed
/// `leave` that was made before returns its first reply: every other change is one a replay
/// finds already made.
pub(super) fn update(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    update: &ThreadUpdate,
) -> Result<ThreadsReply, CommsError> {
    let ks = reader.ks;
    let (actor, id) = (&caller.actor, &update.thread);
    let leave_key = caller
        .replay_key
        .as_ref()
        .filter(|_| update.action == ThreadAction::Leave)
        .map(|key| keys::replay_leave(actor, id, key));
    if let Some(reply) = reader.replay(leave_key.as_deref())? {
        return Ok(reply);
    }
    let mut record = reader.thread(id)?.ok_or_else(|| not_found(id))?;
    let mut members = reader.members(id)?;
    let principal = actor.principal_id();
    let change = rules::authorize_update(
        id,
        &update.action,
        &record.access(caller, members.contains(&principal)),
        members.len(),
        |principal| members.contains(principal),
    )?;
    let pending = match &update.action {
        ThreadAction::Invite(invited) => reader.pending(actor, invited)?,
        ThreadAction::Join | ThreadAction::Leave | ThreadAction::Archive => Vec::new(),
    };
    match change {
        Change::Nothing => {}
        Change::Join => {
            txn.add_member(ks, id, &principal, record.head);
            members.push(principal);
        }
        Change::Leave => {
            txn.batch.remove(&ks.members, keys::member(id, &principal));
            txn.batch
                .remove(&ks.member_threads, keys::thread_of(principal.as_str(), id));
            for (sibling, _) in reader.positions(id, principal.as_str())? {
                txn.batch.remove(&ks.positions, keys::position(id, &sibling));
                txn.batch
                    .remove(&ks.actor_threads, keys::thread_of(sibling.as_str(), id));
            }
            members.retain(|member| *member != principal);
            if members.is_empty() && !record.archived {
                txn.archive(ks, id, &mut record)?;
            }
        }
        Change::Invite(invited) => {
            for member in invited {
                txn.add_member(ks, id, &member, record.head);
                members.push(member);
            }
        }
        Change::Archive => txn.archive(ks, id, &mut record)?,
    }
    let reply = reply(vec![summarize(reader, id, &record, actor, members)?], pending);
    if let Some(key) = leave_key {
        txn.remember(ks, key, &reply)?;
    }
    Ok(reply)
}

/// One thread as `actor` sees it, given its `members` as they are after the call.
///
/// The unread count reads message rows without bodies, from the actor's read position to the
/// head. An actor that just left reads as a non-member although the snapshot still holds its
/// member row.
fn summarize(
    reader: &Reader<'_>,
    id: &ThreadId,
    record: &ThreadRecord,
    actor: &Actor,
    mut members: Vec<Principal>,
) -> Result<ThreadSummary, CommsError> {
    let member = members.iter().any(|member| member.as_str() == actor.principal());
    let position = if member { reader.position(id, actor)? } else { None };
    // ~keep A principal that joins in this call has no member row in the snapshot yet: it
    // starts at the head, with nothing unread.
    let read_seq = position.map_or(record.head, |(position, _)| position.read_seq);
    let mut unread = 0;
    if read_seq < record.head {
        for entry in reader.metas(id, read_seq + 1..=record.head) {
            if entry?.1.from != *actor {
                unread += 1;
            }
        }
    }
    members.retain(|member| member.as_str() != actor.principal());
    members.sort();
    Ok(ThreadSummary {
        id: id.clone(),
        subject: record.subject.clone(),
        repo: record.repo.clone(),
        paths: record.paths.clone(),
        open: record.open,
        archived: record.archived,
        creator: record.creator.clone(),
        member,
        unread,
        head: record.head,
        last_activity_micros: record.last_activity_micros,
        peers: members,
    })
}
