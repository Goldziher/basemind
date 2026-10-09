//! Post idempotency: a client-supplied key makes a retried `post` return the original message id
//! instead of appending a second copy. Rows live in the `meta` keyspace (so they survive a daemon
//! restart) and are written in the same batch as the message they describe.

use super::*;

/// How long a key keeps deduplicating. Retries happen within seconds; an hour is generous while
/// keeping the row set small.
pub const IDEMPOTENCY_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// Longest accepted client key, in bytes.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
const IDEM_PREFIX: &[u8] = b"idem\0";

/// Result of a keyed post.
#[derive(Debug)]
pub enum PostOutcome {
    /// The message was appended.
    Stored(u64, MessageMeta),
    /// The key was seen within the window; carries the original message id.
    Duplicate(String),
}

fn idem_key(agent: &AgentId, thread: &ThreadId, key: &str) -> Vec<u8> {
    let mut out = IDEM_PREFIX.to_vec();
    for part in [agent.as_str(), thread.as_str(), key] {
        out.extend_from_slice(part.as_bytes());
        out.push(0);
    }
    out
}

/// Whether `key` is acceptable: non-empty, bounded, printable ASCII (no NUL separator ambiguity).
pub fn valid_idempotency_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_IDEMPOTENCY_KEY_BYTES && key.bytes().all(|b| b.is_ascii_graphic())
}

impl CommsStore {
    /// [`Self::post`] deduplicated on `(meta.from, thread, key)`: a repeat within
    /// [`IDEMPOTENCY_TTL`] returns the original id and appends nothing.
    pub fn post_keyed(
        &self,
        thread: &ThreadId,
        meta: MessageMeta,
        body: MessageBody,
        key: &str,
    ) -> Result<PostOutcome, CommsStoreError> {
        let row_key = idem_key(&meta.from, thread, key);
        let _guard = self.post_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = self.meta.get(&row_key)?
            && let Ok((id, ts)) = rmp_serde::from_slice::<(String, i64)>(&value)
            && now_micros().saturating_sub(ts) < i64::try_from(IDEMPOTENCY_TTL.as_micros()).unwrap_or(i64::MAX)
        {
            return Ok(PostOutcome::Duplicate(id));
        }
        let row = rmp_serde::to_vec(&(meta.id.clone(), now_micros()))?;
        let (seq, meta) = self.post_with_row(thread, meta, body, Some((row_key, row)))?;
        Ok(PostOutcome::Stored(seq, meta))
    }

    /// Drop idempotency rows older than [`IDEMPOTENCY_TTL`].
    pub(super) fn prune_idempotency(&self) -> Result<usize, CommsStoreError> {
        let cutoff = now_micros().saturating_sub(i64::try_from(IDEMPOTENCY_TTL.as_micros()).unwrap_or(i64::MAX));
        let mut batch = self.db.batch();
        let mut pruned = 0usize;
        for guard in self.meta.prefix(IDEM_PREFIX) {
            let (key, value) = guard.into_inner()?;
            let expired = rmp_serde::from_slice::<(String, i64)>(&value).is_ok_and(|(_, ts)| ts < cutoff);
            if expired {
                batch.remove(&self.meta, key.to_vec());
                pruned += 1;
            }
        }
        if pruned > 0 {
            batch.commit()?;
        }
        Ok(pruned)
    }
}
