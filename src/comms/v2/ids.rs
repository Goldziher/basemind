//! Validated identifiers of agent comms v2 (ADR-0021).
//!
//! Every id is a key segment in the store and travels verbatim between the hooks, the CLI, the MCP
//! tools and the daemon, so each one validates where it is built and again at the serde boundary: a
//! malformed id is refused with the offending value instead of becoming a key that nothing can
//! address.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::comms::ids::{AgentId, MAX_ID_LEN};

/// Separator between a principal and its sub-identity, and between a thread and a sequence number.
/// Reserved: no principal, sub-name or thread id may contain it, which keeps both splits unambiguous.
pub const ID_SEPARATOR: char = '.';

/// Prefix of every thread id.
pub const THREAD_ID_PREFIX: &str = "t-";

/// Number of base32 digits after [`THREAD_ID_PREFIX`].
pub const THREAD_ID_DIGITS: usize = 6;

/// Longest sub-identity name, in bytes.
pub const MAX_SUB_NAME_LEN: usize = 32;

/// Longest repository key, in bytes. A key is a normalised remote or `path:<root>`, so this is a
/// path-length bound, not an id-length one.
pub const MAX_REPO_KEY_LEN: usize = 4096;

/// Longest replay key, in bytes: the bound the relay's `relay-<key>` stamps already fit.
pub const MAX_REPLAY_KEY_LEN: usize = 128;

/// Prefix of a repository key derived from a checkout root rather than from a remote.
pub const PATH_REPO_PREFIX: &str = "path:";

/// Lowercase RFC 4648 base32 digits. Lowercase so a thread id stays inside the sub-name alphabet
/// and reads the same in a shell, a log line and a tool argument.
const BASE32_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Bits one base32 digit carries.
const BASE32_BITS: u32 = 5;

/// Mask selecting one base32 digit.
const BASE32_MASK: u32 = (1 << BASE32_BITS) - 1;

/// Longest caller-supplied value echoed back in a refusal or notice, in bytes. The value ends up in
/// a tool reply, so it is bounded.
const MAX_ECHOED_VALUE_LEN: usize = 160;

/// Which identifier a value failed to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdKind {
    /// A [`ThreadId`].
    Thread,
    /// A [`MessageId`].
    Message,
    /// A [`SubName`].
    Sub,
    /// A [`Principal`].
    Principal,
    /// An [`Actor`].
    Actor,
    /// A [`RepoKey`].
    Repo,
    /// A [`ReplayKey`].
    ReplayKey,
}

impl IdKind {
    /// The noun an error message uses for this kind.
    pub fn label(self) -> &'static str {
        match self {
            Self::Thread => "thread id",
            Self::Message => "message id",
            Self::Sub => "sub-identity name",
            Self::Principal => "agent id",
            Self::Actor => "agent id",
            Self::Repo => "repository key",
            Self::ReplayKey => "replay key",
        }
    }

    /// The grammar a value of this kind must satisfy, phrased as the correction to make.
    pub fn expected(self) -> &'static str {
        match self {
            Self::Thread => "`t-` followed by 6 characters of a-z or 2-7, as returned by agent_post or agent_threads",
            Self::Message => "`<thread>.<seq>` such as `t-abcdef.12`, as returned by agent_post or agent_inbox",
            Self::Sub => "1 to 32 characters of a-z, 0-9, `_` or `-`",
            Self::Principal => {
                "1 to 128 characters of A-Z, a-z, 0-9, `_`, `:` or `-`; `.` is reserved for sub-identities"
            }
            Self::Actor => "`<agent>` or `<agent>.<sub>` of at most 128 bytes, the sub being a-z, 0-9, `_` or `-`",
            Self::Repo => "a non-empty repository id or `path:<root>` without control characters",
            Self::ReplayKey => "1 to 128 printable ASCII characters without spaces",
        }
    }
}

/// An identifier that failed validation, with the value that was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("invalid {} {value:?}: expected {}", .kind.label(), .kind.expected())]
pub struct InvalidId {
    /// Which identifier the value was meant to be.
    pub kind: IdKind,
    /// The refused value, cut to a bounded length.
    pub value: String,
}

impl InvalidId {
    /// Record `value` as refused, echoing at most [`bounded_echo`] of it.
    pub fn new(kind: IdKind, value: &str) -> Self {
        Self {
            kind,
            value: bounded_echo(value),
        }
    }
}

/// The part of a caller-supplied `value` a refusal or notice may repeat: cut at a character
/// boundary so an oversized input cannot inflate the reply that reports it.
pub fn bounded_echo(value: &str) -> String {
    let mut end = value.len().min(MAX_ECHOED_VALUE_LEN);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

macro_rules! string_schema {
    ($name:ident, $pattern:literal) => {
        impl rmcp::schemars::JsonSchema for $name {
            /// Inlined, not referenced: a `$ref` into `$defs` is rejected by the Anthropic
            /// `input_schema` subset, which silently drops the whole tool registry (GH #50).
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
                rmcp::schemars::json_schema!({
                    "type": "string",
                    "pattern": $pattern,
                })
            }
        }
    };
}

macro_rules! string_id {
    ($name:ident, $kind:expr, $valid:path) => {
        impl $name {
            /// Parse and validate, returning the refused value on rejection.
            pub fn parse(s: impl Into<String>) -> Result<Self, InvalidId> {
                let s = s.into();
                if $valid(&s) {
                    Ok(Self(s))
                } else {
                    Err(InvalidId::new($kind, &s))
                }
            }

            /// Borrow the validated identifier.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::parse(s).map_err(serde::de::Error::custom)
            }
        }
    };
}

/// Id of a thread: `t-` and six base32 digits.
///
/// Short enough to transcribe, and free of [`ID_SEPARATOR`] so a [`MessageId`] splits in one place.
/// The 30 bits are not a uniqueness guarantee: the store checks a new id for collision and draws
/// again.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ThreadId(String);

string_id!(ThreadId, IdKind::Thread, valid_thread_id);
string_schema!(ThreadId, "^t-[a-z2-7]{6}$");

impl ThreadId {
    /// Number of distinct thread ids: `32^6`.
    pub const SPACE: u32 = 1 << (BASE32_BITS * THREAD_ID_DIGITS as u32);

    /// The thread id for the low 30 bits of `bits`, most significant digit first. Total, so the
    /// store can turn any random draw into a candidate id without a failure path.
    pub fn from_bits(bits: u32) -> Self {
        let mut id = String::with_capacity(THREAD_ID_PREFIX.len() + THREAD_ID_DIGITS);
        id.push_str(THREAD_ID_PREFIX);
        for digit in (0..THREAD_ID_DIGITS as u32).rev() {
            let index = (bits >> (digit * BASE32_BITS)) & BASE32_MASK;
            id.push(char::from(BASE32_ALPHABET[index as usize]));
        }
        Self(id)
    }

    /// The 30 bits this id encodes: the inverse of [`ThreadId::from_bits`].
    pub fn bits(&self) -> u32 {
        self.0.as_bytes()[THREAD_ID_PREFIX.len()..]
            .iter()
            .fold(0, |bits, byte| (bits << BASE32_BITS) | base32_value(*byte).unwrap_or(0))
    }
}

fn base32_value(byte: u8) -> Option<u32> {
    BASE32_ALPHABET
        .iter()
        .position(|digit| *digit == byte)
        .map(|index| index as u32)
}

fn valid_thread_id(s: &str) -> bool {
    s.strip_prefix(THREAD_ID_PREFIX).is_some_and(|digits| {
        digits.len() == THREAD_ID_DIGITS && digits.bytes().all(|byte| base32_value(byte).is_some())
    })
}

/// Name of a sub-identity: what a subagent passes as `as`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SubName(String);

string_id!(SubName, IdKind::Sub, valid_sub_name);
string_schema!(SubName, "^[a-z0-9_-]{1,32}$");

fn valid_sub_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SUB_NAME_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// A session's own identity, before any sub-identity: the existing agent-id grammar without
/// [`ID_SEPARATOR`], so `<principal>.<sub>` can only have been composed by that principal.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Principal(String);

string_id!(Principal, IdKind::Principal, valid_principal);

fn valid_principal(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ID_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-'))
}

/// Key making a retried request return its first result. Always the relay's per-request key, never
/// a tool field: a model that repeats a call means a second message.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ReplayKey(String);

string_id!(ReplayKey, IdKind::ReplayKey, valid_replay_key);

fn valid_replay_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_REPLAY_KEY_LEN && s.bytes().all(|b| b.is_ascii_graphic())
}

/// The repository a thread belongs to: the registry's repository id, or `path:<canonical root>`
/// for a checkout the registry has no id for.
///
/// Two clones of one remote share a key and therefore share threads, by design.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RepoKey(String);

string_id!(RepoKey, IdKind::Repo, valid_repo_key);

impl RepoKey {
    /// The key of a checkout without a registry id. Formats the root exactly as
    /// `crate::git::scope_key` does, so a repository keyed by path there has the same key here.
    pub fn from_root(canonical_root: &Path) -> Result<Self, InvalidId> {
        Self::parse(format!("{PATH_REPO_PREFIX}{}", canonical_root.display()))
    }

    /// Whether the key names a checkout root rather than a remote, so it is not shared by clones.
    pub fn is_path_keyed(&self) -> bool {
        self.0.starts_with(PATH_REPO_PREFIX)
    }
}

fn valid_repo_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_REPO_KEY_LEN && !s.chars().any(char::is_control)
}

/// Who is acting: a principal, or one of its sub-identities.
///
/// On the wire and in the store it is the composed id (`<principal>` or `<principal>.<sub>`), so a
/// recipient, a sender and a caller are all the same type. A session can only build actors from its
/// own principal, which is what stops it acting as another agent.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Actor {
    id: AgentId,
    principal_len: usize,
}

impl Actor {
    /// Compose `principal` with an optional sub-identity. Refused when the composed id is longer
    /// than [`MAX_ID_LEN`], because it could not be a store key.
    pub fn new(principal: &Principal, sub: Option<&SubName>) -> Result<Self, InvalidId> {
        let composed = match sub {
            Some(sub) => format!("{principal}{ID_SEPARATOR}{sub}"),
            None => principal.as_str().to_string(),
        };
        Self::from_parts(composed, principal.as_str().len())
    }

    /// Parse a composed id: a principal, optionally followed by [`ID_SEPARATOR`] and a sub-name.
    pub fn parse(s: impl Into<String>) -> Result<Self, InvalidId> {
        let s = s.into();
        let (principal, sub) = match s.split_once(ID_SEPARATOR) {
            Some((principal, sub)) => (principal, Some(sub)),
            None => (s.as_str(), None),
        };
        if !valid_principal(principal) || sub.is_some_and(|sub| !valid_sub_name(sub)) {
            return Err(InvalidId::new(IdKind::Actor, &s));
        }
        let principal_len = principal.len();
        Self::from_parts(s, principal_len)
    }

    fn from_parts(composed: String, principal_len: usize) -> Result<Self, InvalidId> {
        match AgentId::parse(composed.as_str()) {
            Ok(id) => Ok(Self { id, principal_len }),
            Err(_) => Err(InvalidId::new(IdKind::Actor, &composed)),
        }
    }

    /// The composed id.
    pub fn as_str(&self) -> &str {
        self.id.as_str()
    }

    /// The composed id as the store's agent-id type.
    pub fn agent_id(&self) -> &AgentId {
        &self.id
    }

    /// The principal part: the identity the session resolved to.
    pub fn principal(&self) -> &str {
        &self.id.as_str()[..self.principal_len]
    }

    /// The principal as an id of its own: what is a member of a thread, for all its actors.
    pub fn principal_id(&self) -> Principal {
        Principal(self.principal().to_string())
    }

    /// The sub-identity name, when the actor is one.
    pub fn sub(&self) -> Option<&str> {
        self.id.as_str().get(self.principal_len + ID_SEPARATOR.len_utf8()..)
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Actor {
    type Err = InvalidId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Actor {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Actor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(s).map_err(serde::de::Error::custom)
    }
}

string_schema!(Actor, "^[A-Za-z0-9_:-]+(\\.[a-z0-9_-]{1,32})?$");

/// Id of a message: its thread and its sequence number in that thread, written `<thread>.<seq>`.
///
/// Both halves are in the id, so resolving a reference is a point lookup, and a read position can
/// be reported as the id of the last message read.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId {
    thread: ThreadId,
    seq: u64,
}

impl MessageId {
    /// The id of message `seq` in `thread`. Sequence numbers start at 1 (0 is the read position
    /// "nothing read"), so 0 is refused.
    pub fn new(thread: ThreadId, seq: u64) -> Result<Self, InvalidId> {
        if seq == 0 {
            return Err(InvalidId::new(IdKind::Message, &format!("{thread}{ID_SEPARATOR}0")));
        }
        Ok(Self { thread, seq })
    }

    /// Parse `<thread>.<seq>`. The sequence is canonical decimal: no sign, no leading zero.
    pub fn parse(s: &str) -> Result<Self, InvalidId> {
        let invalid = || InvalidId::new(IdKind::Message, s);
        let (thread, seq) = s.split_once(ID_SEPARATOR).ok_or_else(invalid)?;
        if seq.is_empty() || seq.starts_with('0') || !seq.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let thread = ThreadId::parse(thread).map_err(|_| invalid())?;
        let seq = seq.parse::<u64>().map_err(|_| invalid())?;
        Ok(Self { thread, seq })
    }

    /// The thread the message is in.
    pub fn thread(&self) -> &ThreadId {
        &self.thread
    }

    /// The message's sequence number in its thread, from 1.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{ID_SEPARATOR}{}", self.thread, self.seq)
    }
}

impl FromStr for MessageId {
    type Err = InvalidId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for MessageId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

string_schema!(MessageId, "^t-[a-z2-7]{6}\\.[1-9][0-9]{0,19}$");

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
