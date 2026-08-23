//! The `mode` vocabulary shared by every consolidated MCP tool and CLI group.
//!
//! basemind's surface is nine domains — `code`, `graph`, `git`, `memory`, `web`, `agents`,
//! `workspace`, `shell`, `admin` — each a single MCP tool and a single CLI group. Which operation
//! runs inside a domain is selected by a required `mode` enum rather than by a distinct tool name.
//!
//! # Why one tool per domain
//!
//! Hosts defer MCP tools and surface them only through keyword search, so an agent never reads a
//! list — it searches. Every additional tool name competes for the same query, and a name whose
//! description omits the words an agent would actually type is unreachable in practice even when it
//! registered cleanly. Nine dense descriptions retrieve better than eighty-five thin ones.
//!
//! # Why the schema is hand-written
//!
//! The Anthropic `input_schema` subset rejects `$ref` / `$defs` / `oneOf` / `anyOf` / `allOf`, and
//! the rejection is silent and all-or-nothing: one offending construct anywhere drops the server's
//! ENTIRE tool registry (GH #50, guarded by `tests/mcp_schema_wire.rs`). `#[derive(JsonSchema)]` on
//! an enum with per-variant doc comments emits exactly that — `oneOf: [{const, description}, …]`.
//! So [`define_mode!`] emits a flat `{"type": "string", "enum": [...]}` with the variant docs folded
//! into the description, and opts into `inline_schema()` so it never lands in `$defs`.
//!
//! For the same reason, per-mode parameters are **optional sibling fields validated in the helper**,
//! never a schema-level union. [`reject_unsupported`] is the validator: it names the offending
//! `mode`/field pair instead of ignoring the field, because a silently-ignored parameter reads to an
//! agent as a successful call that did something else.
//!
//! # Why `mode` has no default
//!
//! A default would let an agent omit it and silently get a different operation than it asked for —
//! an empty result rather than an error, which is the failure mode agents are worst at noticing.

use rmcp::ErrorData as McpError;

/// Declare a domain's `mode` enum: the variants, their wire spellings, and the one-line meaning of
/// each. Generates the enum, a `serde` round-trip whose error names every accepted mode, the flat
/// hand-written [`schemars::JsonSchema`] the Anthropic subset requires, and the `domain:mode`
/// telemetry key that keys the savings table, the slow-tool table, and the CLI renderers.
///
/// ```ignore
/// define_mode! {
///     /// What the `web` tool should do.
///     pub enum WebMode {
///         domain: "web",
///         summary: "Web operation to run.",
///         Scrape => "scrape", "fetch one page and index it";
///         Crawl  => "crawl",  "follow links from a seed URL";
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

// The domain vocabularies. Every enum is compiled unconditionally even when its tool is gated off,
// so this file is the one place the whole surface can be read at once and so a feature build never
// changes the spelling of a mode. Which domains are *advertised* is gated, in `domain_modes`. ~keep

define_mode! {
    /// Which web operation the `web` tool should run.
    pub enum WebMode {
        domain: "web",
        summary: "Web operation to run.",
        Scrape => "scrape", "fetch one URL, extract markdown, and index it into the documents store";
        Crawl => "crawl", "follow links breadth-first from a seed URL and index every page";
        Map => "map", "discover a site's URLs from its sitemap and link map without fetching bodies";
    }
}

define_mode! {
    /// Which cache / server-administration operation the `admin` tool should run.
    pub enum AdminMode {
        domain: "admin",
        summary: "Administrative operation to run.",
        Status => "status", "index health for this workspace: file counts, languages, scan age";
        Repo => "repo", "repository identity and layout: root, git remote, branch, view";
        Rescan => "rescan", "re-index changed files, or the whole workspace when no paths are given";
        CacheStats => "cache_stats", "on-disk size and entry counts for the machine-global cache";
        Gc => "gc", "report blobs no live view references — non-destructive, it deletes nothing";
        CacheClear => "cache_clear", "delete this workspace's cached index outright";
        Telemetry => "telemetry", "aggregate recorded tool calls into a usage and token-savings summary";
        Compress => "compress", "shrink a prior tool response for re-use in a smaller context";
        Delta => "delta", "what changed in a response since a named checkpoint";
        Checkpoint => "checkpoint", "name the current response so a later delta can diff against it";
        Waste => "waste", "flag repeated or redundant tool calls in this session";
    }
}

define_mode! {
    /// Which workspace / worktree registry operation the `workspace` tool should run.
    pub enum WorkspaceMode {
        domain: "workspace",
        summary: "Workspace registry operation to run.",
        Workspaces => "workspaces", "every repository the machine daemon has indexed";
        Worktrees => "worktrees", "git worktrees of this repository, with their branches and claims";
        Branches => "branches", "branches known to this repository";
        Claim => "claim", "take ownership of a worktree so another session does not edit it";
        Release => "release", "give up a worktree claim this session holds";
    }
}

define_mode! {
    /// Which memory / document-retrieval operation the `memory` tool should run.
    pub enum MemoryMode {
        domain: "memory",
        summary: "Memory operation to run.",
        Put => "put", "write a durable note other sessions and agents will read";
        Get => "get", "read one memory entry by key";
        List => "list", "enumerate memory entries, newest first";
        Search => "search", "semantic search across stored memory";
        Delete => "delete", "remove a memory entry by key";
        Audit => "audit", "the write history behind a memory entry";
        Documents => "documents", "semantic search over indexed PDFs, Office files and HTML \
                     instead of opening them";
        Mine => "mine", "derive co-change proposals from git history";
        Proposals => "proposals", "list proposals awaiting review";
        Accept => "accept", "accept a proposal into memory";
        Reject => "reject", "reject a proposal";
    }
}

define_mode! {
    /// Which git-history operation the `git` tool should run.
    pub enum GitMode {
        domain: "git",
        summary: "Git history operation to run.",
        Status => "status", "uncommitted work in the tree, split into staged and unstaged";
        Recent => "recent", "the latest commits with their touched paths and summaries";
        Touching => "touching", "commits that modified a given path";
        ByPath => "by_path", "path-filtered commit log";
        Churn => "churn", "files ranked by how often they change";
        Diff => "diff", "a file's textual diff between two revisions";
        DiffOutline => "diff_outline", "which symbols a file gained, lost or changed between revisions";
        Blame => "blame", "who last changed each line of a file";
        BlameSymbol => "blame_symbol", "who last changed one symbol, resolved to its line span";
        SymbolHistory => "symbol_history", "when a symbol's body actually changed, by structural hash";
        Search => "search", "full-text search over commit messages and authors";
    }
}

define_mode! {
    /// Which code-graph operation the `graph` tool should run.
    pub enum GraphMode {
        domain: "graph",
        summary: "Code-graph operation to run.",
        Calls => "calls", "walk the call chain up or down from one definition";
        Neighbors => "neighbors", "the n-hop neighborhood around a symbol";
        Path => "path", "the confidence-weighted shortest path between two symbols";
        Subgraph => "subgraph", "a symbol's neighborhood cut to its most central nodes";
        Communities => "communities", "cluster the graph into its de-facto modules";
        Map => "map", "whole-repo architecture: hub modules by centrality, plus dependency cycles";
        Export => "export", "render the graph as node-link JSON, DOT, Mermaid, GraphML, Cypher or HTML";
        Display => "display", "open a rendered view in a human's desktop viewer";
        Open => "open", "return a live browsable URL for the interactive graph page";
    }
}

define_mode! {
    /// Which headless-terminal operation the `shell` tool should run.
    pub enum ShellMode {
        domain: "shell",
        summary: "Shell session operation to run.",
        Spawn => "spawn", "start a background terminal session running a command";
        Send => "send", "type into a live session";
        Capture => "capture", "read back what a session has printed";
        Kill => "kill", "terminate a session";
        List => "list", "every session the shell daemon currently hosts";
        Broadcast => "broadcast", "type the same input into several sessions at once";
    }
}

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

define_mode! {
    /// Which code-map lookup the `code` tool should run.
    pub enum CodeMode {
        domain: "code",
        summary: "Code-map lookup to run.",
        Outline => "outline", "a file's structure: symbols, lines, signatures — read this instead of the file";
        Symbols => "symbols", "find a definition by name across every indexed file";
        Grep => "grep", "pattern search across the workspace, filtered by language or path";
        Files => "files", "enumerate indexed files";
        Find => "find", "locate a file by a fuzzy fragment of its name or path";
        Definition => "definition", "resolve a reference position to the definition it binds to";
        References => "references", "every call site of a name";
        Callers => "callers", "callers of one specific definition, disambiguated by path";
        Implementations => "implementations", "types implementing or inheriting a trait, interface or base class";
        Dependents => "dependents", "what imports a given module";
        Expand => "expand", "one symbol's raw source body — the inverse of an outline entry";
        Semantic => "semantic", "search code by meaning; returns pointers, fetch bodies with `chunk`";
        Chunk => "chunk", "fetch one code chunk's source body";
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

/// Every domain this build advertises, paired with the modes it accepts.
///
/// Feature-gated exactly like the router's sub-router merges, so the list can never name a domain
/// the server does not serve. `tests/cli_parity.rs` walks it to prove the MCP and CLI surfaces stay
/// a strict bijection at `(domain, mode)` granularity — consolidation moved the operations from
/// tool names into modes, so parity has to be checked there or it silently stops covering them.
pub fn domain_modes() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        (AdminMode::DOMAIN, AdminMode::ALL_MODES),
        (CodeMode::DOMAIN, CodeMode::ALL_MODES),
        (GitMode::DOMAIN, GitMode::ALL_MODES),
        (GraphMode::DOMAIN, GraphMode::ALL_MODES),
        (MemoryMode::DOMAIN, MemoryMode::ALL_MODES),
        #[cfg(all(feature = "shells", any(unix, windows)))]
        (ShellMode::DOMAIN, ShellMode::ALL_MODES),
        #[cfg(feature = "crawl")]
        (WebMode::DOMAIN, WebMode::ALL_MODES),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        (AgentsMode::DOMAIN, AgentsMode::ALL_MODES),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        (WorkspaceMode::DOMAIN, WorkspaceMode::ALL_MODES),
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
        assert!(reject_unsupported("web", "scrape", &[("max_depth", false), ("limit", false)]).is_ok());

        let error = reject_unsupported("web", "scrape", &[("max_depth", true), ("limit", true)])
            .expect_err("supplied inapplicable fields must fail");
        let message = error.message.to_string();
        assert!(message.contains("`web` mode `scrape` does not accept"), "{message}");
        assert!(
            message.contains("`max_depth`") && message.contains("`limit`"),
            "{message}"
        );
    }
}
