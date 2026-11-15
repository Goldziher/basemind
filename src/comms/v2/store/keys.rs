//! Key encodings of the comms v2 store.
//!
//! The conventions are those of `crate::index::keys`: a variable-length component that is followed
//! by another one is `u16`-big-endian length-prefixed, so one value is never a prefix of a longer
//! one, and every number is big-endian, so byte order is numeric order. Two things keep the keys
//! short: a thread id is its 30 bits as a `u32`, fixed-width and therefore never prefixed, and a
//! variable-length component in last position is written bare.

use crate::comms::v2::api::{Actor, Principal, ReplayKey, RepoKey, ThreadId};

/// Bytes of an encoded thread id.
const THREAD_LEN: usize = 4;

/// Bytes of an encoded sequence number, post position or timestamp.
const U64_LEN: usize = 8;

/// Bytes of a `messages` / `bodies` key.
const MESSAGE_KEY_LEN: usize = THREAD_LEN + U64_LEN;

/// Bytes of a `post_order` key.
const ORDER_KEY_LEN: usize = U64_LEN + U64_LEN;

/// Flips the sign bit, which maps `i64` order onto unsigned byte order.
const SIGN_BIT: u64 = 1 << 63;

/// `repo_threads` state byte of an active thread.
const STATE_ACTIVE: u8 = 0;

/// `repo_threads` state byte of an archived thread.
const STATE_ARCHIVED: u8 = 1;

/// `replays` tag of a post that created a thread: scoped to the actor.
const REPLAY_NEW_THREAD: u8 = 0;

/// `replays` tag of a post into a thread: scoped to the actor and the thread.
const REPLAY_POST: u8 = 1;

/// `replays` tag of a marking inbox read: scoped to the actor.
const REPLAY_INBOX: u8 = 2;

/// `replays` tag of a `leave`: scoped to the actor and the thread.
const REPLAY_LEAVE: u8 = 3;

/// Which records an `expiry` entry is for. The tag leads the key, so each kind is one contiguous
/// range ordered by time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Expiry {
    /// A thread, at its last activity. Payload: the `threads` key.
    Thread = 1,
    /// A replay entry, at the time it was written. Payload: the `replays` key.
    Replay = 2,
    /// A seen record, at the time it was written. Payload: the `seen` key.
    Seen = 3,
    /// A thread that has had more posts than a thread keeps messages, at time zero: where the
    /// sweep drops the oldest. Payload: the `threads` key.
    OverCap = 4,
}

fn timestamp(ts_micros: i64) -> [u8; U64_LEN] {
    ((ts_micros as u64) ^ SIGN_BIT).to_be_bytes()
}

fn parse_timestamp(bytes: &[u8]) -> Option<i64> {
    let bytes: [u8; U64_LEN] = bytes.try_into().ok()?;
    Some((u64::from_be_bytes(bytes) ^ SIGN_BIT) as i64)
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// `u16:len ‖ bytes`. Agent ids are at most 128 bytes and repository keys at most 4096, so the
/// length always fits.
fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    debug_assert!(
        bytes.len() <= usize::from(u16::MAX),
        "comms key component over the u16 ceiling"
    );
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// `threads`: `thread:u32`.
pub(super) fn thread(id: &ThreadId) -> [u8; THREAD_LEN] {
    id.bits().to_be_bytes()
}

/// The thread id a `threads` key, or the thread component of another key, encodes.
pub(super) fn parse_thread(bytes: &[u8]) -> Option<ThreadId> {
    let bits = u32::from_be_bytes(bytes.try_into().ok()?);
    (bits < ThreadId::SPACE).then(|| ThreadId::from_bits(bits))
}

/// `messages` and `bodies`: `thread:u32 ‖ seq:u64`. A range over one thread walks its messages
/// oldest first.
pub(super) fn message(thread_id: &ThreadId, seq: u64) -> [u8; MESSAGE_KEY_LEN] {
    let mut key = [0; MESSAGE_KEY_LEN];
    key[..THREAD_LEN].copy_from_slice(&thread(thread_id));
    key[THREAD_LEN..].copy_from_slice(&seq.to_be_bytes());
    key
}

/// The sequence number of a `messages` key.
pub(super) fn parse_message_seq(key: &[u8]) -> Option<u64> {
    if key.len() != MESSAGE_KEY_LEN {
        return None;
    }
    parse_u64(&key[THREAD_LEN..])
}

/// `post_order`: `ts:u64 ‖ order:u64`, a message's stamp and then its store-wide post position.
/// The stamp leads because a sweep needs the messages oldest stamp first, and stamps follow post
/// positions only inside one thread
/// ([`post_timestamp`](crate::comms::v2::rules::post_timestamp)); the post position keeps two
/// messages stamped alike apart.
pub(super) fn order(ts_micros: i64, order: u64) -> [u8; ORDER_KEY_LEN] {
    let mut key = [0; ORDER_KEY_LEN];
    key[..U64_LEN].copy_from_slice(&timestamp(ts_micros));
    key[U64_LEN..].copy_from_slice(&order.to_be_bytes());
    key
}

/// What a `post_order` row says: a message, and when it was stamped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OrderEntry {
    /// The thread the message is in.
    pub thread: ThreadId,
    /// Its sequence number there.
    pub seq: u64,
    /// When it was stamped.
    pub ts_micros: i64,
}

/// Decode a `post_order` row: its key, and its value, which is the message's `messages` key.
pub(super) fn parse_order_row(key: &[u8], value: &[u8]) -> Option<OrderEntry> {
    if key.len() != ORDER_KEY_LEN || value.len() != MESSAGE_KEY_LEN {
        return None;
    }
    Some(OrderEntry {
        thread: parse_thread(&value[..THREAD_LEN])?,
        seq: parse_u64(&value[THREAD_LEN..])?,
        ts_micros: parse_timestamp(&key[..U64_LEN])?,
    })
}

/// `thread:u32 ‖ id`: a `members` or `positions` key, and the prefix of the `positions` keys of
/// a principal's actors. The thread's prefix is [`thread`].
fn in_thread(thread_id: &ThreadId, id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(THREAD_LEN + id.len());
    key.extend_from_slice(&thread(thread_id));
    key.extend_from_slice(id.as_bytes());
    key
}

/// `members`: `thread:u32 ‖ principal`.
pub(super) fn member(thread_id: &ThreadId, principal: &Principal) -> Vec<u8> {
    in_thread(thread_id, principal.as_str())
}

/// The principal of a `members` key.
pub(super) fn parse_member(key: &[u8]) -> Option<Principal> {
    Principal::parse(std::str::from_utf8(key.get(THREAD_LEN..)?).ok()?).ok()
}

/// `positions`: `thread:u32 ‖ actor`.
pub(super) fn position(thread_id: &ThreadId, actor: &Actor) -> Vec<u8> {
    in_thread(thread_id, actor.as_str())
}

/// Prefix of the `positions` keys of the actors of `principal` in a thread. A longer principal
/// that starts with this one shares it.
pub(super) fn positions_prefix(thread_id: &ThreadId, principal: &str) -> Vec<u8> {
    in_thread(thread_id, principal)
}

/// The actor of a `positions` key.
pub(super) fn parse_position(key: &[u8]) -> Option<Actor> {
    Actor::parse(std::str::from_utf8(key.get(THREAD_LEN..)?).ok()?).ok()
}

/// Prefix of the `member_threads` keys of a principal, or the `actor_threads` keys of an actor:
/// `u16:len ‖ id`.
pub(super) fn threads_of_prefix(owner: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + owner.len() + THREAD_LEN);
    write_len_prefixed(&mut key, owner.as_bytes());
    key
}

/// `member_threads` and `actor_threads`: `u16:len ‖ id ‖ thread:u32`.
pub(super) fn thread_of(owner: &str, thread_id: &ThreadId) -> Vec<u8> {
    let mut key = threads_of_prefix(owner);
    key.extend_from_slice(&thread(thread_id));
    key
}

/// Prefix of the `repo_threads` keys of `repo` in one state: `u16:len ‖ repo ‖ state:u8`.
pub(super) fn repo_threads_prefix(repo: &RepoKey, archived: bool) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + repo.as_str().len() + 1 + THREAD_LEN);
    write_len_prefixed(&mut key, repo.as_str().as_bytes());
    key.push(if archived { STATE_ARCHIVED } else { STATE_ACTIVE });
    key
}

/// `repo_threads`: `u16:len ‖ repo ‖ state:u8 ‖ thread:u32`.
pub(super) fn repo_thread(repo: &RepoKey, archived: bool, thread_id: &ThreadId) -> Vec<u8> {
    let mut key = repo_threads_prefix(repo, archived);
    key.extend_from_slice(&thread(thread_id));
    key
}

/// The thread of a `member_threads`, `actor_threads` or `repo_threads` key: its last four bytes.
pub(super) fn parse_trailing_thread(key: &[u8]) -> Option<ThreadId> {
    parse_thread(key.get(key.len().checked_sub(THREAD_LEN)?..)?)
}

fn replay(tag: u8, actor: &Actor, thread_id: Option<&ThreadId>, key: &ReplayKey) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 2 + actor.as_str().len() + THREAD_LEN + key.as_str().len());
    out.push(tag);
    write_len_prefixed(&mut out, actor.as_str().as_bytes());
    if let Some(thread_id) = thread_id {
        out.extend_from_slice(&thread(thread_id));
    }
    out.extend_from_slice(key.as_str().as_bytes());
    out
}

/// `replays`, for a post that creates a thread: `0 ‖ u16:len ‖ agent ‖ key`.
pub(super) fn replay_new_thread(actor: &Actor, key: &ReplayKey) -> Vec<u8> {
    replay(REPLAY_NEW_THREAD, actor, None, key)
}

/// `replays`, for a post into a thread: `1 ‖ u16:len ‖ agent ‖ thread:u32 ‖ key`.
pub(super) fn replay_post(actor: &Actor, thread_id: &ThreadId, key: &ReplayKey) -> Vec<u8> {
    replay(REPLAY_POST, actor, Some(thread_id), key)
}

/// `replays`, for a marking inbox read: `2 ‖ u16:len ‖ agent ‖ key`.
pub(super) fn replay_inbox(actor: &Actor, key: &ReplayKey) -> Vec<u8> {
    replay(REPLAY_INBOX, actor, None, key)
}

/// `replays`, for a `leave`: `3 ‖ u16:len ‖ agent ‖ thread:u32 ‖ key`.
pub(super) fn replay_leave(actor: &Actor, thread_id: &ThreadId, key: &ReplayKey) -> Vec<u8> {
    replay(REPLAY_LEAVE, actor, Some(thread_id), key)
}

/// `seen`: the agent id.
pub(super) fn seen(actor: &Actor) -> &[u8] {
    actor.as_str().as_bytes()
}

/// Prefix of every `expiry` key of one kind.
pub(super) fn expiry_prefix(kind: Expiry) -> [u8; 1] {
    [kind as u8]
}

/// `expiry`: `kind:u8 ‖ ts:u64 ‖ payload`, the payload being the key of the record the entry is for.
pub(super) fn expiry(kind: Expiry, ts_micros: i64, payload: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + U64_LEN + payload.len());
    key.push(kind as u8);
    key.extend_from_slice(&timestamp(ts_micros));
    key.extend_from_slice(payload);
    key
}

/// The `expiry` entry that marks a thread as over its message cap.
pub(super) fn over_cap(thread_id: &ThreadId) -> Vec<u8> {
    expiry(Expiry::OverCap, 0, &thread(thread_id))
}

/// The time and the payload of an `expiry` key.
pub(super) fn parse_expiry(key: &[u8]) -> Option<(i64, &[u8])> {
    let ts_micros = parse_timestamp(key.get(1..1 + U64_LEN)?)?;
    Some((ts_micros, &key[1 + U64_LEN..]))
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
