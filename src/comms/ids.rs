//! Validated identifier newtypes for agent-to-agent comms.
//!
//! [`AgentId`] and [`ThreadId`] are short, opaque handles that double as **key segments** in
//! the comms Fjall store. They are length-bounded and restricted to a NUL-free ASCII
//! alphabet so they can be embedded in length-prefixed composite keys without ambiguity, and
//! they validate at the serde boundary (`Deserialize` runs [`AgentId::parse`]) so a malformed
//! id from an MCP client is rejected with a clear error instead of corrupting a key.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

/// Maximum identifier length in bytes. Ids are handles, not free text.
pub const MAX_ID_LEN: usize = 128;

/// Why an identifier was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The identifier was the empty string.
    #[error("identifier is empty")]
    Empty,
    /// The identifier was longer than [`MAX_ID_LEN`] bytes.
    #[error("identifier exceeds {MAX_ID_LEN} bytes")]
    TooLong,
    /// The identifier contained a byte outside the allowed alphabet.
    #[error("identifier contains an invalid character (allowed: A-Z a-z 0-9 '.' '_' ':' '-')")]
    InvalidChar,
}

/// Validate an identifier against the shared rules: non-empty, `<= MAX_ID_LEN` bytes, and
/// drawn only from `[A-Za-z0-9._:-]` (which excludes NUL by construction).
fn validate(s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return Err(IdError::InvalidChar);
    }
    Ok(())
}

macro_rules! id_newtype {
    ($name:ident, $schema_name:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Parse and validate an identifier, returning [`IdError`] on rejection.
            pub fn parse(s: impl Into<String>) -> Result<Self, IdError> {
                let s = s.into();
                validate(&s)?;
                Ok(Self(s))
            }

            /// Borrow the validated identifier.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume into the inner `String`.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
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

        impl rmcp::schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $schema_name.into()
            }
            fn json_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
                rmcp::schemars::json_schema!({
                    "type": "string",
                    "pattern": r"^[A-Za-z0-9._:-]+$",
                    "minLength": 1,
                    "maxLength": 128,
                })
            }
        }
    };
}

id_newtype!(
    AgentId,
    "AgentId",
    "Validated identity handle for an opaque agent (e.g. from `BASEMIND_AGENT_ID`)."
);
id_newtype!(ThreadId, "ThreadId", "Validated id of a comms thread.");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_identifiers() {
        for s in ["agent-1", "claude_code", "team:backend", "a.b.c", "A1", "x"] {
            assert!(AgentId::parse(s).is_ok(), "{s} should be valid");
            assert!(ThreadId::parse(s).is_ok(), "{s} should be valid");
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(AgentId::parse(""), Err(IdError::Empty));
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert_eq!(AgentId::parse(long), Err(IdError::TooLong));
        assert!(AgentId::parse("a".repeat(MAX_ID_LEN)).is_ok());
    }

    #[test]
    fn rejects_invalid_chars_including_nul_and_separators() {
        for bad in ["has space", "slash/", "nul\0byte", "emoji😀", "new\nline", "a\tb"] {
            assert_eq!(
                AgentId::parse(bad),
                Err(IdError::InvalidChar),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn serializes_as_plain_string() {
        let id = AgentId::parse("agent-7").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"agent-7\"");
    }

    #[test]
    fn deserialize_validates() {
        let ok: ThreadId = serde_json::from_str("\"backend-team\"").unwrap();
        assert_eq!(ok.as_str(), "backend-team");
        assert!(serde_json::from_str::<ThreadId>("\"bad id\"").is_err());
        assert!(serde_json::from_str::<ThreadId>("\"\"").is_err());
    }

    #[test]
    fn from_str_roundtrips_display() {
        let id: AgentId = "node.7".parse().unwrap();
        assert_eq!(id.to_string(), "node.7");
    }
}
