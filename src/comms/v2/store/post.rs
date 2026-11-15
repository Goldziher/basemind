//! Posting: a message into a thread, and the thread with it when the post creates one.

use super::records::{MessageMeta, Reader, ThreadRecord, encode};
use super::{Txn, keys};
use crate::comms::v2::api::{
    Caller, CommsError, MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MessageId, PostBody, PostReply,
    PostTarget, RepoKey, ThreadId, cap_archive_notice,
};
use crate::comms::v2::rules::{self, NewThread};

/// Store one post. A relayed request that was stored before returns its first reply instead.
pub(super) fn post(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    post: PostBody,
) -> Result<PostReply, CommsError> {
    let PostBody { target, body } = post;
    let actor = &caller.actor;
    let key = caller.replay_key.as_ref();
    let (thread, reply_to) = match target {
        PostTarget::NewThread { subject, to, paths } => {
            let replay_key = key.map(|key| keys::replay_new_thread(actor, key));
            if let Some(reply) = reader.replay(replay_key.as_deref())? {
                return Ok(reply);
            }
            let plan = NewThread::plan(actor, &subject, &to, &paths)?;
            let reply = create(txn, reader, caller, plan, &body)?;
            if let Some(replay_key) = replay_key {
                txn.remember(reader.ks, replay_key, &reply)?;
            }
            return Ok(reply);
        }
        PostTarget::Thread(thread) => (thread, None),
        PostTarget::ReplyTo(message) => (message.thread().clone(), Some(message)),
    };
    let replay_key = key.map(|key| keys::replay_post(actor, &thread, key));
    if let Some(reply) = reader.replay(replay_key.as_deref())? {
        return Ok(reply);
    }
    let reply = into_thread(txn, reader, caller, thread, reply_to, &body)?;
    if let Some(replay_key) = replay_key {
        txn.remember(reader.ks, replay_key, &reply)?;
    }
    Ok(reply)
}

/// Make room for a new thread in `repo` when it is at its cap, by staging the archiving of the
/// active thread whose newest post is the oldest. Returns the notice that says so.
fn archive_for_cap(txn: &mut Txn<'_>, reader: &Reader<'_>, repo: &RepoKey) -> Result<Option<String>, CommsError> {
    let active = reader.repo_threads(repo, false)?;
    if active.len() < MAX_ACTIVE_THREADS_PER_REPO {
        return Ok(None);
    }
    let mut oldest: Option<(ThreadId, ThreadRecord)> = None;
    for id in active {
        if let Some(record) = reader.thread(&id)?
            && oldest
                .as_ref()
                .is_none_or(|(_, oldest)| record.last_order < oldest.last_order)
        {
            oldest = Some((id, record));
        }
    }
    let Some((id, mut record)) = oldest else {
        return Ok(None);
    };
    txn.archive(reader.ks, &id, &mut record)?;
    Ok(Some(cap_archive_notice(&id)))
}

/// Stage a new thread and its first message.
fn create(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    plan: NewThread,
    body: &str,
) -> Result<PostReply, CommsError> {
    let ks = reader.ks;
    let actor = &caller.actor;
    let pending = reader.pending(actor, &plan.recipients)?;
    let notices = archive_for_cap(txn, reader, &caller.repo)?.into_iter().collect();
    let id = txn.fresh_thread_id(reader)?;
    let (ts_micros, order) = txn.stamp_post(ks, None);
    for member in &plan.members {
        txn.add_member(ks, &id, member, 0);
    }
    let record = ThreadRecord {
        subject: plan.subject,
        repo: caller.repo.clone(),
        paths: plan.paths,
        open: plan.open,
        archived: false,
        creator: actor.clone(),
        head: 1,
        last_activity_micros: ts_micros,
        last_order: order,
    };
    let thread_key = keys::thread(&id);
    txn.batch
        .insert(&ks.repo_threads, keys::repo_thread(&caller.repo, false, &id), []);
    txn.batch.insert(
        &ks.expiry,
        keys::expiry(keys::Expiry::Thread, ts_micros, &thread_key),
        [],
    );
    txn.batch.insert(&ks.threads, thread_key, encode("a thread", &record)?);
    let meta = MessageMeta {
        from: actor.clone(),
        ts_micros,
        order,
        reply_to: None,
    };
    stage_message(txn, reader, &id, 1, &meta, body)?;
    Ok(PostReply {
        id: MessageId::new(id.clone(), 1)?,
        thread: id,
        created: true,
        pending,
        notices,
    })
}

/// Stage a message into an existing thread.
fn into_thread(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    id: ThreadId,
    reply_to: Option<MessageId>,
    body: &str,
) -> Result<PostReply, CommsError> {
    let ks = reader.ks;
    let actor = &caller.actor;
    let record = reader.thread(&id)?;
    let (access, reply_target_stored) = match &record {
        Some(record) => {
            let member = reader.member(&id, &actor.principal_id())?.is_some();
            let stored = match &reply_to {
                Some(message) => reader.meta(&id, message.seq())?.is_some(),
                None => true,
            };
            (Some(record.access(caller, member)), stored)
        }
        None => (None, true),
    };
    rules::authorize_post(&id, access.as_ref(), reply_to.as_ref(), reply_target_stored)?;
    let mut record = record.ok_or_else(|| CommsError::ThreadNotFound { thread: id.clone() })?;

    let (ts_micros, order) = txn.stamp_post(ks, Some(record.last_activity_micros));
    let seq = record.head + 1;
    let thread_key = keys::thread(&id);
    txn.batch.remove(
        &ks.expiry,
        keys::expiry(keys::Expiry::Thread, record.last_activity_micros, &thread_key),
    );
    txn.batch.insert(
        &ks.expiry,
        keys::expiry(keys::Expiry::Thread, ts_micros, &thread_key),
        [],
    );
    if seq > MAX_MESSAGES_PER_THREAD {
        txn.batch.insert(&ks.expiry, keys::over_cap(&id), []);
    }
    record.head = seq;
    record.last_activity_micros = ts_micros;
    record.last_order = order;
    txn.batch.insert(&ks.threads, thread_key, encode("a thread", &record)?);
    let meta = MessageMeta {
        from: actor.clone(),
        ts_micros,
        order,
        reply_to: reply_to.map(|message| message.seq()),
    };
    stage_message(txn, reader, &id, seq, &meta, body)?;
    Ok(PostReply {
        id: MessageId::new(id.clone(), seq)?,
        thread: id,
        created: false,
        pending: Vec::new(),
        notices: Vec::new(),
    })
}

fn stage_message(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    thread: &ThreadId,
    seq: u64,
    meta: &MessageMeta,
    body: &str,
) -> Result<(), CommsError> {
    let ks = reader.ks;
    let key = keys::message(thread, seq);
    txn.batch.insert(&ks.messages, key, encode("a message", meta)?);
    txn.batch.insert(&ks.bodies, key, body.as_bytes());
    txn.batch
        .insert(&ks.post_order, keys::order(meta.ts_micros, meta.order), key);
    Ok(())
}
