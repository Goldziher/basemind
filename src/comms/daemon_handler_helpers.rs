//! Pure helpers for the coordination handlers in `daemon_handlers.rs`: retention-policy bounds,
//! the shared error responses, and cursor/limit arithmetic. Split out to keep that file under the
//! per-file size cap.

use super::cursor::Cursor;
use super::daemon::{DEFAULT_LIMIT, MAX_LIMIT};
use super::daemon_handlers::{MAX_RETENTION_SECS, MIN_RETENTION_SECS};
use super::ids::ThreadId;
use super::protocol::CommsResponse;
use super::store::MessageReferenceResolution;

pub(super) fn validate_retention_policy(
    message_ttl_secs: u64,
    thread_idle_ttl_secs: u64,
    thread_retention_ttl_secs: u64,
    agent_ttl_secs: u64,
    claim_ttl_secs: u64,
) -> Result<(), String> {
    for (name, value) in [
        ("message_ttl_secs", message_ttl_secs),
        ("thread_idle_ttl_secs", thread_idle_ttl_secs),
        ("thread_retention_ttl_secs", thread_retention_ttl_secs),
        ("agent_ttl_secs", agent_ttl_secs),
        ("claim_ttl_secs", claim_ttl_secs),
    ] {
        if !(MIN_RETENTION_SECS..=MAX_RETENTION_SECS).contains(&value) {
            return Err(format!(
                "{name} must be between {MIN_RETENTION_SECS} and {MAX_RETENTION_SECS}"
            ));
        }
    }
    if thread_retention_ttl_secs < thread_idle_ttl_secs {
        return Err("thread_retention_ttl_secs must be greater than or equal to thread_idle_ttl_secs".to_string());
    }
    Ok(())
}

pub(super) fn reference_error(reference: &str, resolution: MessageReferenceResolution) -> CommsResponse {
    let (code, detail) = match resolution {
        MessageReferenceResolution::Malformed => ("malformed_message_ref", "is malformed"),
        MessageReferenceResolution::Missing => ("missing_message_ref", "does not exist"),
        MessageReferenceResolution::Ambiguous => ("ambiguous_message_ref", "matches more than one message"),
        MessageReferenceResolution::Found { .. } => ("invalid_message_ref", "could not be resolved"),
    };
    CommsResponse::Error {
        code: code.to_string(),
        message: format!("message reference `{reference}` {detail}"),
    }
}

pub(super) fn need_hello() -> CommsResponse {
    CommsResponse::Error {
        code: "no_hello".to_string(),
        message: "send Hello before any other request".to_string(),
    }
}

pub(super) fn unknown_thread(thread: &ThreadId) -> CommsResponse {
    CommsResponse::Error {
        code: "unknown_thread".to_string(),
        message: format!("no thread {}", thread.as_str()),
    }
}

pub(super) fn not_creator() -> CommsResponse {
    CommsResponse::Error {
        code: "not_creator".to_string(),
        message: "only the thread creator may manage membership or archive it".to_string(),
    }
}

pub(super) fn not_member(thread: &ThreadId) -> CommsResponse {
    CommsResponse::Error {
        code: "not_member".to_string(),
        message: format!("agent is not a member of thread {}", thread.as_str()),
    }
}

/// Rows scanned past the page limit when counting a thread's unread remainder.
pub(super) const INBOX_UNREAD_SCAN_SLACK: usize = 500;

pub(super) fn clamp_limit(limit: Option<u32>) -> usize {
    usize::try_from(limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)).unwrap_or(DEFAULT_LIMIT as usize)
}

pub(super) fn decode_after(cursor: Option<&Cursor>, thread: &str) -> u64 {
    match cursor.and_then(|c| c.decode().ok()) {
        Some(pos) if pos.thread == thread || pos.thread.is_empty() => pos.seq,
        _ => 0,
    }
}

/// Whether a message with `ts_micros` passes the optional recency cutoff.
pub(super) fn keep_since(ts_micros: i64, since_micros: Option<i64>) -> bool {
    match since_micros {
        Some(cut) => ts_micros >= cut,
        None => true,
    }
}

/// Record the highest delivered `seq` for `thread` in a small per-page accumulator.
pub(super) fn upsert_high(acc: &mut Vec<(ThreadId, u64)>, thread: &ThreadId, seq: u64) {
    if let Some(entry) = acc.iter_mut().find(|(t, _)| t == thread) {
        if seq > entry.1 {
            entry.1 = seq;
        }
    } else {
        acc.push((thread.clone(), seq));
    }
}
