//! Wire-shape contract for `tools/list`: every tool's `inputSchema`, its `_meta`, its description,
//! and the size of the whole payload (ADR-0018).
//!
//! The schema rules are the regression guard for the silent all-or-nothing registration failure
//! (GH #50): basemind's whole 78-tool surface connected cleanly to Claude Code — `tools/list` returned every tool and the
//! handshake reported `hasTools: true` — yet **zero** tools reached the agent's registry, with no
//! error logged anywhere. The cause was JSON Schema constructs the Anthropic `input_schema` subset
//! does not accept:
//!
//! - `oneOf` / `anyOf` / `allOf` at a property type (`RelPath` hand-emitted a
//!   `oneOf: [string, {bytes: [u8]}]` — serde's non-UTF-8 escape hatch leaking onto the wire), and
//! - `$ref` into `$defs`, often carrying a sibling `description`, which many validators treat as a
//!   conflict rather than a composition.
//!
//! rmcp builds the schema generator itself (`SchemaSettings::draft2020_12()`, `inline_subschemas:
//! false`) and exposes no seam to configure it, so a global setting cannot fix this: each newtype
//! that would otherwise land in `$defs` must opt into inlining via `JsonSchema::inline_schema()`.
//! This test is the durable guard — the bug shipped because nothing asserted the shape that actually
//! crosses the wire.

use rmcp::ServiceExt;

/// Schema keywords that must never reach the wire. Every one of these has been observed to make a
/// strict tool-schema validator reject the payload.
const FORBIDDEN: [&str; 5] = ["$ref", "$defs", "oneOf", "anyOf", "allOf"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_input_schema_is_flat_and_inlined() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "pub fn seed() {}\n").expect("seed file");

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");
    let tools = service.list_all_tools().await.expect("list tools");

    assert!(!tools.is_empty(), "the server must expose tools to validate");

    let mut offenders: Vec<String> = Vec::new();
    for tool in &tools {
        let schema = serde_json::to_string(&tool.input_schema).expect("serialize inputSchema");
        let hits: Vec<&str> = FORBIDDEN.iter().copied().filter(|kw| schema.contains(*kw)).collect();
        if !hits.is_empty() {
            offenders.push(format!("{} -> {}", tool.name, hits.join(", ")));
        }
    }

    assert!(
        offenders.is_empty(),
        "{} of {} tools emit schema constructs the Anthropic input_schema subset rejects, which \
         silently drops the ENTIRE tool registry (GH #50). Give each offending newtype an \
         `inline_schema() -> true` and a scalar `json_schema`:\n  {}",
        offenders.len(),
        tools.len(),
        offenders.join("\n  "),
    );

    let _ = service.cancel().await;
}

/// Hosts truncate server instructions at a fixed budget (Claude Code at 4,096 characters) and say
/// so only in their own debug log; the agent never learns that part of what the server told it was
/// thrown away. Truncation keeps the HEAD, so overflow silently deletes whatever sits at the END.
const INSTRUCTIONS_CEILING: usize = 4096;

/// Byte ceiling for the verb tools in the serialized `tools/list` (about 5,000 tokens at 4 bytes per
/// token). Every session pays this before the first call, and the surface it replaced cost 25 KB
/// for the same jobs. A description or schema that pushes past it has to be shortened, not the
/// ceiling raised.
const VERB_TOOLS_BYTE_CEILING: usize = 20_000;

/// Ceiling for the mode-dispatched domain tool a `comms` build still adds (`agents`). It predates
/// ADR-0018 and is replaced by its own verb tools in later work; until then this only stops it
/// growing.
#[cfg(feature = "comms")]
const DOMAIN_TOOLS_BYTE_CEILING: usize = 8_000;

/// The mode-dispatched domain tools still advertised by a `comms` build.
const DOMAIN_TOOLS: [&str; 1] = ["agents"];

/// Fields each verb tool cannot run without. Everything else in its schema must be optional.
const REQUIRED: [(&str, &[&str]); 15] = [
    ("code_outline", &["path"]),
    ("code_symbols", &["name"]),
    ("code_read", &["path"]),
    ("code_grep", &["pattern"]),
    ("code_search", &["query"]),
    ("code_refs", &["name"]),
    ("find_files", &[]),
    ("docs_search", &["query"]),
    ("git_log", &[]),
    ("git_blame", &["path"]),
    ("git_diff", &[]),
    ("memory_save", &["key", "value"]),
    ("memory_search", &[]),
    ("index_status", &[]),
    ("index_rescan", &[]),
];

async fn list_tools() -> (tempfile::TempDir, Vec<rmcp::model::Tool>, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("lib.rs"), "pub fn seed() {}\n").expect("seed file");
    let transport = basemind::mcp::serve_in_memory(dir.path(), "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");
    let tools = service.list_all_tools().await.expect("list tools");
    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    let _ = service.cancel().await;
    (dir, tools, instructions)
}

fn verb_tools(tools: &[rmcp::model::Tool]) -> Vec<&rmcp::model::Tool> {
    tools
        .iter()
        .filter(|tool| !DOMAIN_TOOLS.contains(&tool.name.as_ref()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instructions_stay_under_the_client_ceiling() {
    let (_dir, _tools, instructions) = list_tools().await;

    assert!(
        !instructions.is_empty(),
        "the server must send instructions; an empty string means the agent gets no guidance at all"
    );
    assert!(
        instructions.chars().count() <= INSTRUCTIONS_CEILING,
        "server instructions are {} chars, over the {INSTRUCTIONS_CEILING}-char client ceiling: the tail \
         would be silently dropped on every connection. Trim the text; per-tool detail belongs in the tool \
         descriptions.",
        instructions.chars().count(),
    );
    for tool in [
        "code_outline",
        "code_symbols",
        "code_grep",
        "code_refs",
        "git_log",
        "docs_search",
    ] {
        assert!(
            instructions.contains(tool),
            "instructions must name `{tool}`: {instructions}"
        );
    }
    for retired in ["code mode", "git mode", "memory mode", "admin mode", "required mode"] {
        assert!(
            !instructions.contains(retired),
            "instructions still describe the mode-dispatched surface (`{retired}`): {instructions}"
        );
    }
}

/// Every advertised tool carries a human-readable `title`, so clients can show a label that is not
/// the bare identifier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_has_a_title() {
    let (_dir, tools, _) = list_tools().await;
    let untitled: Vec<_> = tools
        .iter()
        .filter(|tool| tool.title.as_deref().is_none_or(|title| title.trim().is_empty()))
        .map(|tool| tool.name.to_string())
        .collect();
    assert!(untitled.is_empty(), "tools without a title: {untitled:?}");
}

/// The advertised surface is exactly the ADR-0018 verb tools (plus the one domain tool a `comms`
/// build still carries). In particular none of the replaced domain tools is left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_advertised_tools_are_exactly_the_verb_tools() {
    let (_dir, tools, _) = list_tools().await;
    let mut advertised: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    advertised.sort();

    for retired in ["code", "git", "memory", "admin", "workspace"] {
        assert!(
            !advertised.iter().any(|name| name == retired),
            "the mode-dispatched `{retired}` tool is still advertised: {advertised:?}"
        );
    }

    let mut expected: Vec<String> = basemind::mcp::params::VERB_TOOLS
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert_eq!(expected.len(), REQUIRED.len(), "REQUIRED must list every verb tool");
    if cfg!(feature = "comms") {
        expected.extend(DOMAIN_TOOLS.iter().map(|name| name.to_string()));
    }
    expected.sort();
    assert_eq!(advertised, expected);
}

/// A host that defers MCP tools loads only their names until a tool-search round trip; this marker
/// keeps the schema loaded from the start of the session. It is spent on the verb tools only: the
/// mode-dispatched domain tool stays deferred, so its bytes are not paid on every session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_verb_tools_are_marked_always_load() {
    let (_dir, tools, _) = list_tools().await;
    let verbs = verb_tools(&tools);
    assert_eq!(verbs.len(), REQUIRED.len(), "every verb tool is advertised");
    for tool in &tools {
        let wire = serde_json::to_value(tool).expect("serialize tool");
        let marker = wire.pointer("/_meta/anthropic~1alwaysLoad");
        if DOMAIN_TOOLS.contains(&tool.name.as_ref()) {
            assert_eq!(
                marker,
                None,
                "`{}` is a domain tool and must stay deferred, got {}",
                tool.name,
                wire.get("_meta").cloned().unwrap_or_default(),
            );
        } else {
            assert_eq!(
                marker,
                Some(&serde_json::Value::Bool(true)),
                "`{}` must serialize `_meta[\"anthropic/alwaysLoad\"]: true`, got {}",
                tool.name,
                wire.get("_meta").cloned().unwrap_or_default(),
            );
        }
    }
    if cfg!(feature = "comms") {
        assert_eq!(
            tools.len() - verbs.len(),
            DOMAIN_TOOLS.len(),
            "a comms build advertises the domain tool, so the deferred branch above ran"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_stays_under_the_byte_ceiling() {
    let (_dir, tools, _) = list_tools().await;
    let size = |tools: &[&rmcp::model::Tool]| serde_json::to_string(tools).expect("serialize tools").len();

    let verbs = verb_tools(&tools);
    let verb_bytes = size(&verbs);
    let mut per_tool: Vec<(usize, &str)> = verbs.iter().map(|tool| (size(&[*tool]), tool.name.as_ref())).collect();
    per_tool.sort_unstable_by(|a, b| b.cmp(a));
    println!(
        "tools/list: {} verb tools, {verb_bytes} bytes (~{} tokens); whole list {} bytes",
        verbs.len(),
        verb_bytes / 4,
        serde_json::to_string(&tools).expect("serialize tools").len(),
    );
    assert!(
        verb_bytes <= VERB_TOOLS_BYTE_CEILING,
        "the verb tools serialize to {verb_bytes} bytes (~{} tokens), over the {VERB_TOOLS_BYTE_CEILING}-byte \
         ceiling. Shorten a description or a field doc; largest first: {per_tool:?}",
        verb_bytes / 4,
    );

    #[cfg(feature = "comms")]
    {
        let domains: Vec<&rmcp::model::Tool> = tools
            .iter()
            .filter(|tool| DOMAIN_TOOLS.contains(&tool.name.as_ref()))
            .collect();
        assert_eq!(domains.len(), DOMAIN_TOOLS.len());
        let domain_bytes = size(&domains);
        assert!(
            domain_bytes <= DOMAIN_TOOLS_BYTE_CEILING,
            "`agents` serializes to {domain_bytes} bytes, over {DOMAIN_TOOLS_BYTE_CEILING}"
        );
    }
}

/// Required arguments are required in the schema, so a host can validate a call before sending it,
/// and nothing else is: an `Option` field must not leak into `required`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_fields_are_required_in_the_schema() {
    let (_dir, tools, _) = list_tools().await;
    for (name, expected) in REQUIRED {
        let tool = tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("`{name}` is not advertised"));
        let mut required: Vec<&str> = tool
            .input_schema
            .get("required")
            .and_then(|value| value.as_array())
            .map(|items| items.iter().filter_map(|item| item.as_str()).collect())
            .unwrap_or_default();
        required.sort_unstable();
        let mut expected: Vec<&str> = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(required, expected, "`{name}` schema `required`");
        assert_eq!(
            tool.input_schema.get("type").and_then(|value| value.as_str()),
            Some("object"),
            "`{name}` schema must be an object"
        );
    }
}

/// Each description opens with when to use the tool and carries one complete example call that the
/// tool's own argument parse accepts. Comparing the example's keys with the schema is not enough: a
/// value of the wrong type or outside a field's vocabulary has the right keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_verb_tool_description_carries_a_valid_example() {
    let (_dir, tools, _) = list_tools().await;
    for tool in verb_tools(&tools) {
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.starts_with("Use "),
            "`{}` must open with when to use it: {description}",
            tool.name
        );
        let (_, example) = description
            .split_once("Example: ")
            .unwrap_or_else(|| panic!("`{}` description has no `Example: {{…}}`: {description}", tool.name));
        let arguments: serde_json::Value = serde_json::from_str(example.trim())
            .unwrap_or_else(|error| panic!("`{}` example is not JSON ({error}): {example}", tool.name));
        let arguments = arguments.as_object().expect("example arguments are an object");
        let properties = tool
            .input_schema
            .get("properties")
            .and_then(|value| value.as_object())
            .cloned()
            .unwrap_or_default();
        for key in arguments.keys() {
            assert!(
                properties.contains_key(key),
                "`{}` example passes `{key}`, which its schema does not define",
                tool.name
            );
        }
        let required = tool
            .input_schema
            .get("required")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        for field in required.iter().filter_map(|field| field.as_str()) {
            assert!(
                arguments.contains_key(field),
                "`{}` example omits the required `{field}`",
                tool.name
            );
        }
        let parsed =
            basemind::mcp::params::check_arguments(tool.name.as_ref(), serde_json::Value::Object(arguments.clone()))
                .unwrap_or_else(|| panic!("`{}` is not a verb tool", tool.name));
        assert_eq!(parsed, Ok(()), "`{}` rejects its own example {example}", tool.name);
    }
}

/// The check above runs the real parse: an example with the right keys and a wrong value is refused.
#[test]
fn the_example_check_rejects_a_wrongly_typed_value() {
    let wrong_type = basemind::mcp::params::check_arguments("code_grep", serde_json::json!({ "pattern": 7 }));
    assert!(matches!(wrong_type, Some(Err(_))), "{wrong_type:?}");
    let wrong_word =
        basemind::mcp::params::check_arguments("code_refs", serde_json::json!({ "name": "x", "relation": "users" }));
    assert!(matches!(wrong_word, Some(Err(_))), "{wrong_word:?}");
    assert_eq!(
        basemind::mcp::params::check_arguments("no_such_tool", serde_json::json!({})),
        None
    );
}

/// Annotations are exact per tool, so a host that gates on read-only can allow `index_status` and
/// `git_log` without allowing `index_rescan`. No verb tool is destructive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verb_tool_annotations_are_exact() {
    let (_dir, tools, _) = list_tools().await;
    for tool in verb_tools(&tools) {
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("`{}` has no annotations", tool.name));
        let writes = matches!(tool.name.as_ref(), "memory_save" | "index_rescan");
        assert_eq!(
            annotations.read_only_hint,
            Some(!writes),
            "`{}` read_only_hint",
            tool.name
        );
        assert_eq!(
            annotations.destructive_hint,
            Some(false),
            "`{}` destructive_hint",
            tool.name
        );
        assert_eq!(
            annotations.idempotent_hint,
            Some(true),
            "`{}` idempotent_hint: a repeated call changes nothing further",
            tool.name
        );
        assert_eq!(
            annotations.open_world_hint,
            Some(false),
            "`{}` open_world_hint: every verb tool works on this machine's index only",
            tool.name
        );
        assert!(
            tool.output_schema.is_none(),
            "`{}` declares an output schema; the response structs nest, which the flat-schema rule and the \
             byte ceiling both rule out",
            tool.name
        );
    }
}
