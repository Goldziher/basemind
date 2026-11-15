//! Posting: a message into a thread, and the thread with it when the post creates one.

use super::records::{MessageMeta, Reader, ThreadRecord, encode};
use super::{Txn, keys};
use crate::comms::v2::api::{
    Actor, Caller, CommsError, MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MessageId, PostBody, PostReply,
    PostTarget, RepoKey, ThreadId,
};
use crate::comms::v2::rules::{self, NewThread, Position};

/// Store one post. A relayed request that was stored before returns its first reply instead.
pub(super) fn post(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    post: PostBody,
) -> Result<PostReply, CommsError> {
    let PostBody { target, body } = post;
    let actor = &caller.actor;
    let (replay_key, reply) = match target {
        PostTarget::NewThread { subject, to, paths } => {
            let replay_key = caller
                .replay_key
                .as_ref()
                .map(|key| keys::replay_new_thread(actor, key));
            if let Some(reply) = replayed(reader, replay_key.as_deref())? {
                return Ok(reply);
            }
            let plan = NewThread::plan(actor, &subject, &to, &paths)?;
            let pending = reader.pending(actor, &plan.recipients)?;
            rules::check_thread_cap(&caller.repo, active_threads(reader, &caller.repo)?)?;
            let id = txn.fresh_thread_id(reader)?;
            (replay_key, create(txn, reader, caller, id, plan, pending, &body)?)
        }
        PostTarget::Thread(thread) => into_thread(txn, reader, caller, thread, None, &body)?,
        PostTarget::ReplyTo(message) => {
            into_thread(txn, reader, caller, message.thread().clone(), Some(message), &body)?
        }
    };
    if let Some(replay_key) = replay_key {
        txn.remember(reader, replay_key, &reply)?;
    }
    Ok(reply)
}

fn replayed(reader: &Reader<'_>, replay_key: Option<&[u8]>) -> Result<Option<PostReply>, CommsError> {
    match replay_key {
        Some(key) => reader.replay(key),
        None => Ok(None),
    }
}

/// Active threads of `repo` that have not expired, counted no further than the cap. Their index
/// entries are counted first, and the threads themselves are read only when those alone would
/// reach the cap, since an entry outlives its thread until a sweep.
fn active_threads(reader: &Reader<'_>, repo: &RepoKey) -> Result<usize, CommsError> {
    let indexed = reader.repo_threads(repo, false)?;
    if indexed.len() < MAX_ACTIVE_THREADS_PER_REPO {
        return Ok(indexed.len());
    }
    let mut active = 0;
    for id in &indexed {
        if reader.thread(id)?.is_some() {
            active += 1;
            if active == MAX_ACTIVE_THREADS_PER_REPO {
                break;
            }
        }
    }
    Ok(active)
}

/// Stage a new thread and its first message.
fn create(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    id: ThreadId,
    plan: NewThread,
    pending: Vec<Actor>,
    body: &str,
) -> Result<PostReply, CommsError> {
    let ks = reader.ks;
    let actor = &caller.actor;
    let (ts_micros, order) = txn.stamp_post(ks);
    for member in plan.recipients.iter().chain([actor]) {
        txn.add_member(ks, &id, member, Position::START)?;
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
        head_from: actor.clone(),
        prev_other_seq: 0,
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
        notices: Vec::new(),
    })
}

/// Stage a message into an existing thread. Returns the replay key the reply is to be kept
/// under, when the request was relayed.
fn into_thread(
    txn: &mut Txn<'_>,
    reader: &Reader<'_>,
    caller: &Caller,
    id: ThreadId,
    reply_to: Option<MessageId>,
    body: &str,
) -> Result<(Option<Vec<u8>>, PostReply), CommsError> {
    let ks = reader.ks;
    let actor = &caller.actor;
    let replay_key = caller.replay_key.as_ref().map(|key| keys::replay_post(actor, &id, key));
    if let Some(reply) = replayed(reader, replay_key.as_deref())? {
        return Ok((None, reply));
    }
    let record = reader.thread(&id)?;
    let (access, reply_target_stored) = match &record {
        Some(record) => {
            let member = reader.position(&id, actor)?.is_some();
            let stored = match &reply_to {
                Some(message) => reader.meta(&id, message.seq())?.is_some(),
                None => true,
            };
            (Some(record.access(caller, member)), stored)
        }
        None => (None, true),
    };
    rules::authorize_post(&id, access.as_ref(), reply_to.as_ref(), reply_target_stored)?;
    let record = record.ok_or_else(|| CommsError::ThreadNotFound { thread: id.clone() })?;

    let (ts_micros, order) = txn.stamp_post(ks);
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
    let record = record.posted(actor, seq, ts_micros);
    txn.batch.insert(&ks.threads, thread_key, encode("a thread", &record)?);
    let meta = MessageMeta {
        from: actor.clone(),
        ts_micros,
        order,
        reply_to: reply_to.map(|message| message.seq()),
    };
    stage_message(txn, reader, &id, seq, &meta, body)?;
    if let Some(evicted) = seq.checked_sub(MAX_MESSAGES_PER_THREAD).filter(|seq| *seq > 0) {
        evict(txn, reader, &id, evicted)?;
    }
    let reply = PostReply {
        id: MessageId::new(id.clone(), seq)?,
        thread: id,
        created: false,
        pending: Vec::new(),
        notices: Vec::new(),
    };
    Ok((replay_key, reply))
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
    txn.batch.insert(
        &ks.post_order,
        keys::order(meta.order),
        keys::order_entry(thread, seq, meta.ts_micros),
    );
    Ok(())
}

/// Stage the removal of the message a thread's cap pushes out. Stored sequence numbers are
/// contiguous up to the head, so the one to go is always exactly the cap behind the new head.
fn evict(txn: &mut Txn<'_>, reader: &Reader<'_>, thread: &ThreadId, seq: u64) -> Result<(), CommsError> {
    let ks = reader.ks;
    let Some(meta) = reader.stored_meta(thread, seq)? else {
        return Ok(());
    };
    let key = keys::message(thread, seq);
    txn.batch.remove(&ks.messages, key);
    txn.batch.remove(&ks.bodies, key);
    txn.batch.remove(&ks.post_order, keys::order(meta.order));
    Ok(())
}
