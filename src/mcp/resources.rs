//! MCP resources (`resources/list`, `resources/templates/list`, `resources/read`).
//!
//! A deliberately small surface that re-exposes existing tool output as addressable, on-demand
//! content. Nothing here is part of the always-loaded prompt: clients list resources lazily, so the
//! cost to a session that never touches them is zero. Every read delegates to the same `run_*`
//! dispatcher the matching tool uses, so a resource body is byte-for-byte the tool result.
//!
//! | URI                          | Backing tool call             |
//! |------------------------------|-------------------------------|
//! | `basemind://status`          | `admin` mode `status`         |
//! | `basemind://repo/map`        | `graph` mode `map`            |
//! | `basemind://outline/{path}`  | `code` mode `outline`         |
//! | `basemind://memory/{key}`    | `memory` mode `get`           |
//!
//! `{path}` is repo-relative and validated before it reaches the index (no absolute paths, `..`,
//! backslashes, NUL, empty segments); the downstream code-only/document-tier policy still applies,
//! so an outline of a non-code file errors "not indexed". `resources/subscribe` is intentionally not
//! implemented: the status payload changes with every scan, but a correct implementation needs a
//! per-peer subscription registry and a scan-completion hook on every writer path.

use rmcp::ErrorData as McpError;
use rmcp::model::{
    CacheScope, CallToolResult, ContentBlock, ListResourceTemplatesResult, ListResourcesResult, ReadResourceResult,
    Resource, ResourceContents, ResourceTemplate,
};
use serde_json::{Value, json};

use super::BasemindServer;
use super::admission::WorkClass;
use super::server_handler::LIST_CACHE_TTL_MS;

const SCHEME: &str = "basemind://";
const URI_STATUS: &str = "basemind://status";
const URI_REPO_MAP: &str = "basemind://repo/map";
const TEMPLATE_OUTLINE: &str = "basemind://outline/{path}";
const TEMPLATE_MEMORY: &str = "basemind://memory/{key}";
const MIME_JSON: &str = "application/json";

/// A parsed `basemind://` resource URI.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResourceUri {
    Status,
    RepoMap,
    Outline(String),
    Memory(String),
}

/// Parse and validate a resource URI. The error string is client-facing.
pub(super) fn parse_uri(uri: &str) -> Result<ResourceUri, String> {
    let rest = uri
        .strip_prefix(SCHEME)
        .ok_or_else(|| format!("unsupported resource URI {uri:?}: expected the basemind:// scheme"))?;
    if rest.contains(['?', '#']) {
        return Err(format!("resource URI {uri:?} must not carry a query or fragment"));
    }
    match rest {
        "status" => return Ok(ResourceUri::Status),
        "repo/map" => return Ok(ResourceUri::RepoMap),
        _ => {}
    }
    if let Some(encoded) = rest.strip_prefix("outline/") {
        return Ok(ResourceUri::Outline(validate_rel_path(&percent_decode(encoded)?)?));
    }
    if let Some(encoded) = rest.strip_prefix("memory/") {
        let key = percent_decode(encoded)?;
        if key.is_empty() || key.contains('\0') {
            return Err("memory resource key must be non-empty and free of NUL".to_string());
        }
        return Ok(ResourceUri::Memory(key));
    }
    Err(format!("unknown resource {uri:?}"))
}

/// Reject anything that is not a plain repo-relative forward-slash path.
fn validate_rel_path(path: &str) -> Result<String, String> {
    if path.is_empty() {
        return Err("outline resource path is empty".to_string());
    }
    if path.contains(['\0', '\\']) {
        return Err(format!("path {path:?} contains a NUL or backslash"));
    }
    if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') {
        return Err(format!("path {path:?} must be repo-relative, not absolute"));
    }
    if path.split('/').any(|segment| matches!(segment, "" | "." | "..")) {
        return Err(format!("path {path:?} has an empty, `.` or `..` segment"));
    }
    Ok(path.to_string())
}

/// Decode `%XX` escapes (UTF-8 only). Done before validation so `%2e%2e` cannot smuggle a `..`.
fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input
                .get(index + 1..index + 3)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("invalid percent-escape in {input:?}"))?;
            out.push(hex);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("resource URI {input:?} is not valid UTF-8 once decoded"))
}

/// Percent-encode a repo path for use in an `outline` URI (unreserved chars and `/` pass through).
#[cfg(test)]
pub(super) fn outline_uri(path: &str) -> String {
    let mut uri = String::from("basemind://outline/");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// First text block of a tool result, or an error when the tool reported one.
fn tool_text(result: CallToolResult, uri: &str) -> Result<String, McpError> {
    let text = result.content.iter().find_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    });
    if result.is_error == Some(true) {
        return Err(McpError::invalid_params(
            text.unwrap_or_else(|| format!("reading {uri} failed")),
            None,
        ));
    }
    text.ok_or_else(|| McpError::internal_error(format!("{uri}: tool returned no text content"), None))
}

pub(super) fn static_resources() -> Vec<Resource> {
    vec![
        Resource::new(URI_STATUS, "status")
            .with_title("Index status")
            .with_description("Index health: file counts, languages, lifecycle flags. Same as admin mode status.")
            .with_mime_type(MIME_JSON),
        Resource::new(URI_REPO_MAP, "repo-map")
            .with_title("Repository architecture map")
            .with_description("Hub modules, fan-in/out and cycles, ranked by centrality. Same as graph mode map.")
            .with_mime_type(MIME_JSON),
    ]
}

pub(super) fn resource_templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(TEMPLATE_OUTLINE, "file-outline")
            .with_title("File outline")
            .with_description(
                "Symbols and imports of one indexed code file (repo-relative path). Same as code mode outline.",
            )
            .with_mime_type(MIME_JSON),
        ResourceTemplate::new(TEMPLATE_MEMORY, "memory-entry")
            .with_title("Memory entry")
            .with_description("One shared memory note by key. Same as memory mode get.")
            .with_mime_type(MIME_JSON),
    ]
}

impl BasemindServer {
    pub(super) fn list_resources_result(&self) -> ListResourcesResult {
        ListResourcesResult::with_all_items(static_resources())
            .with_ttl_ms(LIST_CACHE_TTL_MS)
            .with_cache_scope(CacheScope::Public)
    }

    pub(super) fn list_resource_templates_result(&self) -> ListResourceTemplatesResult {
        ListResourceTemplatesResult::with_all_items(resource_templates())
            .with_ttl_ms(LIST_CACHE_TTL_MS)
            .with_cache_scope(CacheScope::Public)
    }

    /// `resources/read`: parse the URI, then run the same dispatcher the equivalent tool call uses.
    pub(super) async fn read_resource_uri(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let parsed =
            parse_uri(uri).map_err(|message| McpError::invalid_params(message, Some(json!({ "uri": uri }))))?;
        let class = if parsed == ResourceUri::RepoMap {
            WorkClass::Heavy
        } else {
            WorkClass::Control
        };
        let _admission = self.heavy_admission.admit(class).await.map_err(McpError::from)?;
        self.state.await_cache_ready().await;
        let state = &self.state;
        let result = match parsed {
            ResourceUri::Status => {
                let params = from_args(json!({ "mode": "status" }))?;
                super::helpers_admin::run_admin(state.clone(), params, None).await?
            }
            ResourceUri::RepoMap => {
                super::helpers_graph::run_graph(state, from_args(json!({ "mode": "map" }))?).await?
            }
            ResourceUri::Outline(path) => {
                super::helpers_code::run_code(state, from_args(json!({ "mode": "outline", "path": path }))?).await?
            }
            ResourceUri::Memory(key) => {
                let result =
                    super::helpers_memory::run_memory(state, from_args(json!({ "mode": "get", "key": key }))?).await?;
                let text = tool_text(result, uri)?;
                if text.trim() == "null" {
                    return Err(McpError::resource_not_found(
                        format!("no memory entry {key:?}"),
                        Some(json!({ "uri": uri })),
                    ));
                }
                return Ok(contents(uri, text));
            }
        };
        Ok(contents(uri, tool_text(result, uri)?))
    }

    /// Completion for resource-template arguments: `outline`'s `path` completes against indexed paths.
    pub(super) fn complete_resource_argument(&self, template_uri: &str, argument: &str, value: &str) -> Vec<String> {
        match (template_uri, argument) {
            (TEMPLATE_OUTLINE, "path") => self.complete_file_paths(value),
            _ => Vec::new(),
        }
    }
}

fn contents(uri: &str, text: String) -> ReadResourceResult {
    ReadResourceResult::new(vec![ResourceContents::text(text, uri).with_mime_type(MIME_JSON)])
}

fn from_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, McpError> {
    serde_json::from_value(args).map_err(|e| McpError::internal_error(format!("resource params: {e}"), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_static_and_templated_uris() {
        assert_eq!(parse_uri("basemind://status"), Ok(ResourceUri::Status));
        assert_eq!(parse_uri("basemind://repo/map"), Ok(ResourceUri::RepoMap));
        assert_eq!(
            parse_uri("basemind://outline/src/a%20b.rs"),
            Ok(ResourceUri::Outline("src/a b.rs".to_string()))
        );
        assert_eq!(
            parse_uri("basemind://memory/skill/x"),
            Ok(ResourceUri::Memory("skill/x".to_string()))
        );
    }

    #[test]
    fn outline_uri_round_trips() {
        let uri = outline_uri("src/weird name#1.rs");
        assert_eq!(
            parse_uri(&uri),
            Ok(ResourceUri::Outline("src/weird name#1.rs".to_string()))
        );
    }

    #[test]
    fn rejects_traversal_and_malformed_paths() {
        for bad in [
            "basemind://outline/",
            "basemind://outline/../etc/passwd",
            "basemind://outline/a/../b.rs",
            "basemind://outline/%2e%2e/secret",
            "basemind://outline/%2E%2E%2Fsecret",
            "basemind://outline//etc/passwd",
            "basemind://outline/%2Fetc/passwd",
            "basemind://outline/a//b.rs",
            "basemind://outline/./a.rs",
            "basemind://outline/a%5Cb.rs",
            "basemind://outline/a%00b.rs",
            "basemind://outline/C:/x.rs",
            "basemind://outline/a.rs?x=1",
            "basemind://outline/%zz",
            "basemind://outline/%ff",
            "basemind://memory/",
            "basemind://nope",
            "file:///etc/passwd",
        ] {
            assert!(parse_uri(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn advertised_templates_and_resources_parse() {
        for resource in static_resources() {
            assert!(parse_uri(&resource.uri).is_ok(), "{}", resource.uri);
        }
        assert_eq!(resource_templates().len(), 2);
    }
}
