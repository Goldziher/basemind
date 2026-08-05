//! Wire-shape contract for every tool's `inputSchema`.
//!
//! Regression guard for the silent all-or-nothing registration failure (GH #50): basemind's whole
//! 78-tool surface connected cleanly to Claude Code — `tools/list` returned every tool and the
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

/// Clients truncate server instructions at a fixed budget and say so only in their own debug log
/// ("Server instructions truncated from 5585 to 2048 chars") — the agent never learns that most of
/// what the server told it was thrown away. Truncation keeps the HEAD, so overflow silently deletes
/// whatever sits at the END: when this text ran ~6.5k, the entire agent-coordination contract was
/// discarded on every connection while appearing to be delivered.
const INSTRUCTIONS_CEILING: usize = 2048;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instructions_stay_under_the_client_ceiling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "pub fn seed() {}\n").expect("seed file");

    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    let service = ().serve(transport).await.expect("rmcp handshake");

    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();

    assert!(
        !instructions.is_empty(),
        "the server must send instructions; an empty string means the agent gets no guidance at all"
    );
    assert!(
        instructions.len() <= INSTRUCTIONS_CEILING,
        "server instructions are {} chars, over the {INSTRUCTIONS_CEILING}-char client ceiling — the \
         last {} chars would be silently dropped on every connection. Trim the text; per-tool routing \
         detail belongs in tool descriptions, which is what deferred-tool search matches on.",
        instructions.len(),
        instructions.len() - INSTRUCTIONS_CEILING,
    );

    let _ = service.cancel().await;
}
