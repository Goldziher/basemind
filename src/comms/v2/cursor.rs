//! Opaque pagination cursors of agent comms v2 (ADR-0021).
//!
//! A cursor is a position plus the identity of the list it is a position in. The list identity is
//! a hash over the caller and everything that selects the list, so a cursor handed to another
//! agent, another selection or another read mode is refused instead of paging the wrong list.
//! Every implementation of [`AgentOps`](super::api::AgentOps) issues and reads cursors through
//! this type, which is what makes one issued by the daemon readable after it restarts.
//!
//! Only reads that mark nothing page with a cursor. An unread read that marks continues from the
//! read position, which is the only cursor that cannot skip a message.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::error::CommsError;
use super::ids::{Actor, RepoKey, ThreadId};
use crate::path::RelPath;

/// Leading character of an inbox cursor.
const INBOX_TAG: char = 'i';

/// Leading character of a thread-listing cursor.
const THREADS_TAG: char = 't';

/// Hex digits of the list identity.
const BINDING_DIGITS: usize = 16;

/// Hex digits of a 64-bit position.
const U64_DIGITS: usize = 16;

/// Start value of FNV-1a (64 bit).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Multiplier of FNV-1a (64 bit).
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Which of the two cursor-paged inbox reads a cursor belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagedRead {
    /// Unread messages, without marking: the tool's `peek`.
    Peek,
    /// Every message, read or not: the tool's `unread: false`.
    History,
}

/// Opaque resume token: pass a reply's `next_cursor` back as `cursor` to get the next page.
///
/// Any text deserializes into one; whether it is a cursor of the request at hand is decided when
/// it is used, by [`Cursor::inbox_position`] or [`Cursor::threads_position`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(String);

impl rmcp::schemars::JsonSchema for Cursor {
    /// Inlined, not referenced: a `$ref` into `$defs` is rejected by the Anthropic
    /// `input_schema` subset, which silently drops the whole tool registry (GH #50).
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Cursor".into()
    }

    fn json_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
        rmcp::schemars::json_schema!({ "type": "string" })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Cursor {
    /// Wrap text a caller passed as `cursor`. Not validated here: see the type's documentation.
    pub fn from_wire(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The cursor as it travels.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The cursor after the message at store-wide post position `order`, for `actor` paging
    /// `thread` (every thread when `None`) in read mode `read`.
    pub fn for_inbox(actor: &Actor, thread: Option<&ThreadId>, read: PagedRead, order: u64) -> Self {
        let binding = inbox_binding(actor, thread, read);
        Self(format!("{INBOX_TAG}{binding:016x}{order:016x}"))
    }

    /// The post position this cursor resumes after, when it was issued to `actor` for the same
    /// selection and read mode. Refused with [`CommsError::InvalidCursor`] otherwise.
    pub fn inbox_position(&self, actor: &Actor, thread: Option<&ThreadId>, read: PagedRead) -> Result<u64, CommsError> {
        let invalid = || CommsError::invalid_cursor(&self.0);
        let rest = self.bound_rest(INBOX_TAG, inbox_binding(actor, thread, read))?;
        if rest.len() != U64_DIGITS {
            return Err(invalid());
        }
        parse_hex(rest).ok_or_else(invalid)
    }

    /// The cursor after the thread whose newest post is at store-wide post position `order`,
    /// for `listing`.
    pub fn for_threads(listing: &ThreadListing<'_>, order: u64) -> Self {
        let binding = listing.binding();
        Self(format!("{THREADS_TAG}{binding:016x}{order:016x}"))
    }

    /// The post position this cursor resumes after, when it was issued for `listing`. Refused
    /// with [`CommsError::InvalidCursor`] otherwise.
    pub fn threads_position(&self, listing: &ThreadListing<'_>) -> Result<u64, CommsError> {
        let invalid = || CommsError::invalid_cursor(&self.0);
        let rest = self.bound_rest(THREADS_TAG, listing.binding())?;
        if rest.len() != U64_DIGITS {
            return Err(invalid());
        }
        parse_hex(rest).ok_or_else(invalid)
    }

    /// What follows the tag and the list identity, when both are the expected ones.
    fn bound_rest(&self, tag: char, binding: u64) -> Result<&str, CommsError> {
        let invalid = || CommsError::invalid_cursor(&self.0);
        let body = self.0.strip_prefix(tag).ok_or_else(invalid)?;
        if !body.is_ascii() || body.len() < BINDING_DIGITS {
            return Err(invalid());
        }
        let (prefix, rest) = body.split_at(BINDING_DIGITS);
        if parse_hex(prefix) != Some(binding) {
            return Err(invalid());
        }
        Ok(rest)
    }
}

/// What selects a thread listing: a cursor continues only the listing it was issued for.
#[derive(Clone, Copy, Debug)]
pub struct ThreadListing<'a> {
    /// Who lists.
    pub actor: &'a Actor,
    /// The repository the caller lists from.
    pub repo: &'a RepoKey,
    /// The normalised subject query, when there is one.
    pub query: Option<&'a str>,
    /// The normalised path filter, when there is one.
    pub path: Option<&'a RelPath>,
    /// Whether archived threads are included.
    pub archived: bool,
}

impl ThreadListing<'_> {
    fn binding(&self) -> u64 {
        let mut hash = fnv1a(FNV_OFFSET, &[THREADS_TAG as u8]);
        hash = field(hash, self.actor.as_str().as_bytes());
        hash = field(hash, self.repo.as_str().as_bytes());
        hash = optional_field(hash, self.query.map(str::as_bytes));
        hash = optional_field(hash, self.path.map(RelPath::as_bytes));
        field(hash, &[u8::from(self.archived)])
    }
}

fn inbox_binding(actor: &Actor, thread: Option<&ThreadId>, read: PagedRead) -> u64 {
    let mode = match read {
        PagedRead::Peek => b'p',
        PagedRead::History => b'h',
    };
    let mut hash = fnv1a(FNV_OFFSET, &[INBOX_TAG as u8, mode]);
    hash = field(hash, actor.as_str().as_bytes());
    optional_field(hash, thread.map(|thread| thread.as_str().as_bytes()))
}

/// FNV-1a over `bytes`, continuing from `hash`. Stable across builds and processes, which a
/// cursor that outlives a daemon restart needs; `ahash` is seeded per process.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hash one length-prefixed field, so `("ab", "c")` and `("a", "bc")` differ.
fn field(hash: u64, bytes: &[u8]) -> u64 {
    let hash = fnv1a(hash, &(bytes.len() as u64).to_le_bytes());
    fnv1a(hash, bytes)
}

/// Hash an optional field so that absent and empty differ.
fn optional_field(hash: u64, bytes: Option<&[u8]>) -> u64 {
    match bytes {
        Some(bytes) => field(fnv1a(hash, &[1]), bytes),
        None => fnv1a(hash, &[0]),
    }
}

/// Lowercase hex only: `from_str_radix` alone would also accept a sign and uppercase, giving one
/// position several spellings.
fn parse_hex(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
