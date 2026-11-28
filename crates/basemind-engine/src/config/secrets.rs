//! Credential types for the deprecated `[llm]` section: a tri-state API key that never serialises
//! its cleartext, and the newtype for a resolved secret.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Tri-state API key wrapper. Serde-deserialised as `null` (`Unset`), a plain
/// string (`Literal`), or a `{ env = "NAME" }` table (`Env`).
///
/// `Serialize` is implemented manually — the `Literal` variant NEVER emits its
/// cleartext secret. It serialises to the marker string `"<redacted>"` so any
/// downstream pipeline that round-trips this enum through JSON / TOML (e.g.
/// the config validator at `src/config/mod.rs`) cannot leak credentials into
/// logs, snapshot tests, or error messages. Deserialisation is unchanged:
/// loading a TOML config with a literal `api_key = "sk-..."` still produces
/// `ApiKey::Literal(...)`. The redaction is one-way by design.
#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
#[serde(untagged)]
pub enum ApiKey {
    /// Literal value baked into config. Strongly discouraged for production —
    /// `Env` keeps secrets out of source control.
    Literal(String),
    /// `{ env = "OPENAI_API_KEY" }` — resolved at load time via `std::env::var`.
    Env { env: String },
    /// Missing / `null` — every LLM-backed feature treats this as "disabled".
    #[default]
    Unset,
}

impl PartialEq for ApiKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Literal(a), Self::Literal(b)) => a == b,
            (Self::Env { env: a }, Self::Env { env: b }) => a == b,
            (Self::Unset, Self::Unset) => true,
            _ => false,
        }
    }
}

impl serde::Serialize for ApiKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Literal(_) => serializer.serialize_str("<redacted>"),
            Self::Env { env } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("EnvRef", 1)?;
                s.serialize_field("env", env)?;
                s.end()
            }
            Self::Unset => serializer.serialize_none(),
        }
    }
}

impl ApiKey {
    /// Resolve the credential. `Literal` returns its inner value as-is; `Env`
    /// performs an `env::var` lookup and returns `None` if the variable is
    /// missing or empty; `Unset` always returns `None`.
    pub fn resolve(&self) -> Option<SecretString> {
        self.resolve_with(|name| std::env::var(name).ok())
    }

    /// [`Self::resolve`] with `lookup` reading the `Env` variable in place of the process
    /// environment.
    fn resolve_with(&self, lookup: impl FnOnce(&str) -> Option<String>) -> Option<SecretString> {
        match self {
            Self::Literal(s) if !s.is_empty() => Some(SecretString(s.clone())),
            Self::Literal(_) => None,
            Self::Env { env } => lookup(env).filter(|v| !v.is_empty()).map(SecretString),
            Self::Unset => None,
        }
    }
}

/// Newtype wrapping a resolved secret. `Debug` and `Display` always render
/// `"<redacted>"` so spans / panics never leak the value. Use `expose()` when
/// you genuinely need the raw bytes at an API boundary.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a raw string as a secret. The caller is responsible for ensuring
    /// the bytes are not already on disk in plaintext.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrow the underlying secret. Use only at the boundary that consumes the
    /// credential (e.g. building an HTTP authorization header).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"<redacted>\"")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_redacts_in_debug() {
        let s = SecretString::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "\"<redacted>\"");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn api_key_unset_resolves_to_none() {
        assert!(ApiKey::Unset.resolve().is_none());
    }

    #[test]
    fn api_key_literal_resolves_to_value() {
        let k = ApiKey::Literal("sk-test".to_string());
        let resolved = k.resolve().expect("literal resolves");
        assert_eq!(resolved.expose(), "sk-test");
    }

    #[test]
    fn api_key_literal_empty_resolves_to_none() {
        assert!(ApiKey::Literal(String::new()).resolve().is_none());
    }

    #[test]
    fn api_key_env_reads_the_named_variable() {
        let k = ApiKey::Env {
            env: "BASEMIND_TEST_API_KEY".to_string(),
        };
        let resolved = k
            .resolve_with(|name| (name == "BASEMIND_TEST_API_KEY").then(|| "value-123".to_string()))
            .expect("env resolves");
        assert_eq!(resolved.expose(), "value-123");
    }

    #[test]
    fn api_key_env_missing_or_empty_resolves_to_none() {
        let k = ApiKey::Env {
            env: "BASEMIND_TEST_API_KEY".to_string(),
        };
        assert!(k.resolve_with(|_| None).is_none());
        assert!(k.resolve_with(|_| Some(String::new())).is_none());
    }

    #[test]
    fn api_key_resolve_reads_the_process_environment() {
        let k = ApiKey::Env {
            env: "PATH".to_string(),
        };
        let expected = std::env::var("PATH").ok().filter(|value| !value.is_empty());
        assert_eq!(k.resolve().map(|secret| secret.expose().to_string()), expected);
    }

    #[test]
    fn api_key_deserialises_literal_string() {
        let k: ApiKey = serde_json::from_str("\"sk-test\"").expect("parse");
        match k {
            ApiKey::Literal(s) => assert_eq!(s, "sk-test"),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn api_key_deserialises_env_table() {
        let k: ApiKey = serde_json::from_str(r#"{"env":"OPENAI_API_KEY"}"#).expect("parse");
        match k {
            ApiKey::Env { env } => assert_eq!(env, "OPENAI_API_KEY"),
            other => panic!("expected Env, got {other:?}"),
        }
    }

    #[test]
    fn api_key_literal_never_serializes_cleartext() {
        let key = ApiKey::Literal("sk-supersecret".to_string());
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(!json.contains("sk-supersecret"), "raw secret leaked: {json}");
        assert!(json.contains("<redacted>"), "redaction marker missing: {json}");
    }

    #[test]
    fn api_key_env_serializes_env_name_only() {
        let key = ApiKey::Env {
            env: "OPENAI_API_KEY".to_string(),
        };
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(json.contains("OPENAI_API_KEY"), "env name missing: {json}");
        assert!(json.contains("\"env\""), "env field missing: {json}");
    }

    #[test]
    fn api_key_unset_serializes_to_null() {
        let key = ApiKey::Unset;
        let json = serde_json::to_string(&key).expect("serialize");
        assert_eq!(json, "null");
    }
}
