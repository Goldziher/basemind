//! The `mode` vocabulary of the one domain tool that is still mode-dispatched: `agents`.
//!
//! ADR-0018 replaced the mode-dispatched `code`, `git`, `memory` and `admin` domains with one tool
//! per job (`src/mcp/types_tools.rs`). The agent-coordination surface keeps its domain shape until
//! its own redesign lands, so this file now carries only what that tool needs: the enum macro, its
//! vocabulary, and the per-mode field validator.
//!
//! # Why the schema is hand-written
//!
//! The Anthropic `input_schema` subset rejects `$ref` / `$defs` / `oneOf` / `anyOf` / `allOf`, and
//! the rejection is silent and all-or-nothing: one offending construct anywhere drops the server's
//! ENTIRE tool registry (GH #50, guarded by `tests/mcp_schema_wire.rs`). `#[derive(JsonSchema)]` on
//! an enum with per-variant doc comments emits exactly that: `oneOf: [{const, description}, …]`.
//! So [`define_mode!`] emits a flat `{"type": "string", "enum": [...]}` with the variant docs folded
//! into the description, and opts into `inline_schema()` so it never lands in `$defs`.
//!
//! For the same reason, per-mode parameters are optional sibling fields validated in the helper,
//! never a schema-level union. [`reject_unsupported`] is the validator: it names the offending
//! `mode`/field pair instead of ignoring the field.

use rmcp::ErrorData as McpError;

/// Declare a domain's `mode` enum: the variants, their wire spellings, and the one-line meaning of
/// each. Generates the enum, a `serde` round-trip whose error names every accepted mode, the flat
/// hand-written [`schemars::JsonSchema`] the Anthropic subset requires, and the `domain:mode`
/// telemetry key that keys the savings table, the slow-tool table, and the CLI renderers.
///
/// ```ignore
/// define_mode! {
///     /// What the `agents` tool should do.
///     pub enum AgentsMode {
///         domain: "agents",
///         summary: "Coordination operation to run.",
///         Post  => "post",  "send a message to a thread";
///         Inbox => "inbox", "unread messages across your joined threads";
///     }
/// }
/// ```
macro_rules! define_mode {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $name:ident {
            domain: $domain:literal,
            summary: $summary:literal,
            $($variant:ident => $wire:literal, $doc:literal;)+
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                #[doc = $doc]
                $variant,
            )+
        }

        impl $name {
            /// The domain this mode selects within — the MCP tool name and the CLI group name.
            $vis const DOMAIN: &'static str = $domain;

            /// Every mode of this domain, in declaration order.
            $vis const ALL: &'static [$name] = &[$($name::$variant),+];

            /// Wire spellings of [`Self::ALL`], in the same order. The CLI-parity test walks this
            /// to prove every MCP mode has a resolving `basemind <domain> <mode>` subcommand.
            $vis const ALL_MODES: &'static [&'static str] = &[$($wire),+];

            /// This mode's wire spelling.
            $vis const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $wire),+ }
            }

            /// `domain:mode` — the key telemetry, savings estimates, the slow-tool table, and the
            /// CLI renderers are all keyed on, so per-operation granularity survives consolidation.
            $vis const fn telemetry_key(self) -> &'static str {
                match self { $($name::$variant => concat!($domain, ":", $wire)),+ }
            }

            /// Parse a wire spelling, or fail with a `-32602` that names every accepted mode.
            $vis fn parse(raw: &str) -> Result<Self, rmcp::ErrorData> {
                Self::from_wire(raw).ok_or_else(|| rmcp::ErrorData::invalid_params(Self::unknown_message(raw), None))
            }

            fn from_wire(raw: &str) -> Option<Self> {
                match raw {
                    $($wire => Some($name::$variant),)+
                    _ => None,
                }
            }

            fn unknown_message(raw: &str) -> String {
                format!(
                    "unknown mode `{raw}` for `{}`; expected {}",
                    $domain,
                    Self::ALL_MODES.join("|"),
                )
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            /// Hand-written so a bad mode reports every accepted spelling. serde's derived message
            /// for a rejected variant is passed through verbatim by rmcp as the `-32602` text, so
            /// this is the only place the agent's error wording can be set.
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(deserializer)?;
                Self::from_wire(&raw).ok_or_else(|| serde::de::Error::custom(Self::unknown_message(&raw)))
            }
        }

        impl rmcp::schemars::JsonSchema for $name {
            /// Inlined rather than `$ref`'d into `$defs` — see the module docs and GH #50.
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            /// A flat string enum. The derive would turn the variant docs into `oneOf`, which the
            /// Anthropic `input_schema` subset rejects — silently, and for the whole registry — so
            /// the docs are folded into `description` instead.
            fn json_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
                rmcp::schemars::json_schema!({
                    "type": "string",
                    "enum": [$($wire),+],
                    "description": concat!(
                        $summary,
                        " One of:",
                        $(" `", $wire, "` — ", $doc, ";",)+
                    ),
                })
            }
        }
    };
}

// Every enum is compiled unconditionally even when its tool is gated off, so a feature build never
// changes the spelling of a mode. Which domains are *advertised* is gated, in `domain_modes`. ~keep

define_mode! {
    /// Which multi-agent coordination operation the `agents` tool should run.
    pub enum AgentsMode {
        domain: "agents",
        summary: "Agent coordination operation to run.",
        Register => "register", "publish or update this agent's identity card with the broker";
        List => "list", "the agents the broker knows, optionally restricted to one thread";
        ThreadStart => "thread_start", "open a thread addressed by subject, path-glob and/or members";
        ThreadList => "thread_list", "threads discoverable to you: membership, cwd path-match or subject";
        Join => "join", "join a thread so its messages reach your inbox";
        Leave => "leave", "leave a thread you joined";
        Members => "members", "who belongs to a thread";
        AddMember => "add_member", "add an agent to a thread you created";
        RemoveMember => "remove_member", "remove an agent from a thread you created";
        Archive => "archive", "close a thread; creator or human admin only";
        Post => "post", "send a message to a thread";
        History => "history", "a thread's messages as front-matter only; bodies come from `message`";
        Message => "message", "read one message body by id — the only path to a body";
        Inbox => "inbox", "unread messages across your joined threads, front-matter only";
        Ack => "ack", "clear messages you have read from your inbox";
        Wait => "wait", "block until a peer posts, or until the timeout elapses";
        Cleanup => "cleanup", "preview or apply stale agent, claim, message and thread retention";
        Status => "status", "active and stale agent counts plus the last maintenance time";
    }
}

/// Fail a call that passed parameters the selected mode does not accept.
///
/// `present` pairs each inapplicable field name with whether the caller actually supplied it. Every
/// offender is reported at once so a caller fixes one call rather than discovering fields one
/// round-trip at a time. Ignoring the field instead would be worse than erroring: the agent reads a
/// successful response and believes the parameter took effect.
pub fn reject_unsupported(domain: &str, mode: &str, present: &[(&str, bool)]) -> Result<(), McpError> {
    let offenders: Vec<&str> = present
        .iter()
        .filter_map(|(field, supplied)| supplied.then_some(*field))
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    Err(McpError::invalid_params(
        format!(
            "`{domain}` mode `{mode}` does not accept {}",
            offenders
                .iter()
                .map(|field| format!("`{field}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        None,
    ))
}

/// Every mode-dispatched domain this build advertises, paired with the modes it accepts.
///
/// Feature-gated exactly like the router's sub-router merges, so the list can never name a domain
/// the server does not serve. `tests/cli_parity/` walks it so every `agents` mode keeps a CLI
/// command.
pub fn domain_modes() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        #[cfg(all(feature = "comms", any(unix, windows)))]
        (AgentsMode::DOMAIN, AgentsMode::ALL_MODES),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    define_mode! {
        /// Fixture domain used only to exercise the macro's generated impls.
        pub enum TestMode {
            domain: "fixture",
            summary: "Fixture operation to run.",
            Alpha => "alpha", "the first one";
            Beta => "beta", "the second one";
        }
    }

    #[test]
    fn should_round_trip_every_mode_through_its_wire_spelling() {
        for mode in TestMode::ALL {
            assert_eq!(TestMode::parse(mode.as_str()).expect("parse own spelling"), *mode);
        }
        assert_eq!(TestMode::ALL_MODES, &["alpha", "beta"]);
        assert_eq!(TestMode::DOMAIN, "fixture");
    }

    #[test]
    fn should_name_every_accepted_mode_when_the_mode_is_unknown() {
        let error = TestMode::parse("alfa").expect_err("unknown mode must fail");
        let message = error.message.to_string();
        assert!(message.contains("unknown mode `alfa` for `fixture`"), "{message}");
        assert!(message.contains("alpha|beta"), "{message}");
    }

    #[test]
    fn should_key_telemetry_by_domain_and_mode() {
        assert_eq!(TestMode::Alpha.telemetry_key(), "fixture:alpha");
        assert_eq!(TestMode::Beta.telemetry_key(), "fixture:beta");
    }

    #[test]
    fn should_emit_a_flat_inlined_string_schema() {
        let mut generator = rmcp::schemars::SchemaGenerator::default();
        let schema = serde_json::to_string(&<TestMode as rmcp::schemars::JsonSchema>::json_schema(&mut generator))
            .expect("serialize schema");
        for forbidden in ["$ref", "$defs", "oneOf", "anyOf", "allOf"] {
            assert!(!schema.contains(forbidden), "{forbidden} leaked into {schema}");
        }
        assert!(schema.contains(r#""enum":["alpha","beta"]"#), "{schema}");
        assert!(<TestMode as rmcp::schemars::JsonSchema>::inline_schema());
    }

    #[test]
    fn should_deserialize_from_json_and_report_the_accepted_set_on_failure() {
        assert_eq!(
            serde_json::from_str::<TestMode>(r#""beta""#).expect("deserialize"),
            TestMode::Beta
        );
        let error = serde_json::from_str::<TestMode>(r#""gamma""#).expect_err("unknown variant must fail");
        assert!(error.to_string().contains("alpha|beta"), "{error}");
    }

    #[test]
    fn should_reject_only_the_fields_the_caller_actually_supplied() {
        assert!(reject_unsupported("code", "outline", &[("max_depth", false), ("limit", false)]).is_ok());

        let error = reject_unsupported("code", "outline", &[("max_depth", true), ("limit", true)])
            .expect_err("supplied inapplicable fields must fail");
        let message = error.message.to_string();
        assert!(message.contains("`code` mode `outline` does not accept"), "{message}");
        assert!(
            message.contains("`max_depth`") && message.contains("`limit`"),
            "{message}"
        );
    }
}
