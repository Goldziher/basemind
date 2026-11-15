//! Threads: describing one, listing and discovering them, and the four changes to one.

use ahash::AHashSet;

use super::records::{Reader, ThreadRecord, encode};
use super::{Txn, keys};
use crate::comms::v2::api::{
    Actor, Caller, CommsError, ThreadAction, ThreadFilter, ThreadId, ThreadSummary, ThreadUpdate, ThreadsReply,
};
use crate::comms::v2::rules::{self, Change, Position, Selection};

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
    let position = reader.position(id, &caller.actor)?;
    rules::authorize_describe(id, &record.access(caller, position.is_some()))?;
    let summary = summarize(reader, id, &record, &caller.actor, position, reader.members(id)?)?;
    Ok(reply(vec![summary], Vec::new()))
}

/// List the caller's threads and, when the filter discovers, the open threads of its repository.
///
/// The candidates come from two indexes, `member_threads` for the caller and `repo_threads` for
/// its repository, so the cost is the threads the caller is in plus those of the repository, and
/// only the threads of the returned page are summarised.
pub(super) fn list(reader: &Reader<'_>, caller: &Caller, filter: &ThreadFilter) -> Result<ThreadsReply, CommsError> {
    let actor = &caller.actor;
    let selection = Selection::parse(filter)?;
    let mut matching = Vec::new();
    let mut own = AHashSet::new();
    for id in reader.member_threads(actor)? {
        let Some(record) = reader.thread(&id)? else {
            continue;
        };
        let Some(position) = reader.position(&id, actor)? else {
            continue;
        };
        if selection.selects(&record.access(caller, true), &record.subject, &record.paths) {
            matching.push((record.last_activity_micros, id.clone(), (record, Some(position))));
        }
        own.insert(id);
    }
    if selection.discovers() {
        let states: &[bool] = if selection.includes_archived() {
            &[false, true]
        } else {
            &[false]
        };
        for archived in states {
            for id in reader.repo_threads(&caller.repo, *archived)? {
                if own.contains(&id) {
                    continue;
                }
                let Some(record) = reader.thread(&id)? else {
                    continue;
                };
                if selection.selects(&record.access(caller, false), &record.subject, &record.paths) {
                    matching.push((record.last_activity_micros, id, (record, None)));
                }
            }
        }
    }
    let page = rules::listing_page(
        matching,
        &selection.listing(caller),
        filter.cursor.as_ref(),
        filter.limit,
    )?;
    let mut threads = Vec::with_capacity(page.rows.len());
    for (id, (record, position)) in page.rows {
        threads.push(summarize(reader, &id, &record, actor, position, reader.members(&id)?)?);
    }
    Ok(ThreadsReply {
        threads,
        pending: Vec::new(),
        next_cursor: page.next_cursor,
        notices: Vec::new(),
    })
}

/// Stage one membership or lifecycle change and describe the thread as changed.
pub(super) fn update(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    update: &ThreadUpdate,
) -> Result<ThreadsReply, CommsError> {
    let ks = reader.ks;
    let (actor, id) = (&caller.actor, &update.thread);
    let mut record = reader.thread(id)?.ok_or_else(|| not_found(id))?;
    let mut position = reader.position(id, actor)?;
    let mut members = reader.members(id)?;
    let change = rules::authorize_update(
        id,
        &update.action,
        &record.access(caller, position.is_some()),
        members.len(),
        |agent| members.contains(agent),
    )?;
    let at_head = Position::at_head(record.head);
    let mut pending = Vec::new();
    match change {
        Change::Nothing => {}
        Change::Join => {
            txn.add_member(ks, id, actor, at_head)?;
            position = Some(at_head);
            members.push(actor.clone());
        }
        Change::Leave => {
            txn.batch.remove(&ks.members, keys::member(id, actor));
            txn.batch.remove(&ks.member_threads, keys::member_thread(actor, id));
            position = None;
            members.retain(|member| member != actor);
            if members.is_empty() {
                archive(txn, reader, id, &mut record)?;
            }
        }
        Change::Invite => {
            if let ThreadAction::Invite(invited) = &update.action {
                pending = reader.pending(actor, invited)?;
                for agent in invited {
                    if !members.contains(agent) {
                        txn.add_member(ks, id, agent, at_head)?;
                        members.push(agent.clone());
                    }
                }
            }
        }
        Change::Archive => archive(txn, reader, id, &mut record)?,
    }
    let summary = summarize(reader, id, &record, actor, position, members)?;
    Ok(reply(vec![summary], pending))
}

/// Stage the archiving of a thread: its row, and its place in the repository's index, which
/// keeps active and archived threads apart so the cap counts one range.
fn archive(txn: &mut Txn<'_>, reader: &Reader<'_>, id: &ThreadId, record: &mut ThreadRecord) -> Result<(), CommsError> {
    if record.archived {
        return Ok(());
    }
    let ks = reader.ks;
    record.archived = true;
    txn.batch
        .insert(&ks.threads, keys::thread(id), encode("a thread", record)?);
    txn.batch
        .remove(&ks.repo_threads, keys::repo_thread(&record.repo, false, id));
    txn.batch
        .insert(&ks.repo_threads, keys::repo_thread(&record.repo, true, id), []);
    Ok(())
}

/// One thread as `actor` sees it, given its `members` and the caller's `position` in it.
///
/// The unread count reads message rows without bodies, from the read position to the newest
/// message somebody else posted, so a thread with nothing unread costs no read at all.
fn summarize(
    reader: &Reader<'_>,
    id: &ThreadId,
    record: &ThreadRecord,
    actor: &Actor,
    position: Option<Position>,
    mut members: Vec<Actor>,
) -> Result<ThreadSummary, CommsError> {
    let mut unread = 0;
    if let Some(position) = position {
        let last = record.newest_from_others(actor);
        if position.read_seq < last {
            for entry in reader.metas(id, position.read_seq + 1..=last) {
                if entry?.1.from != *actor {
                    unread += 1;
                }
            }
        }
    }
    members.retain(|member| member != actor);
    members.sort();
    Ok(ThreadSummary {
        id: id.clone(),
        subject: record.subject.clone(),
        repo: record.repo.clone(),
        paths: record.paths.clone(),
        open: record.open,
        archived: record.archived,
        creator: record.creator.clone(),
        member: position.is_some(),
        unread,
        head: record.head,
        last_activity_micros: record.last_activity_micros,
        peers: members,
    })
}
