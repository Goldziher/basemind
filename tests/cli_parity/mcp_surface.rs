//! The live MCP surface: every advertised tool's input schema, parsed.
//!
//! A verb tool has an exact schema of its own (ADR-0018), so the fields it accepts and requires are
//! read straight from it. A mode-dispatched tool would publish one flat schema for all its modes,
//! which cannot say which field belongs to which mode, so its parameters would be compared
//! domain-wide; none is left since `agents` was removed (ADR-0021).

use std::collections::{BTreeMap, BTreeSet};

use basemind::git_cache::GitCache;
use basemind::mcp::BasemindServer;
use basemind::store::{Store, VIEW_WORKING};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use serde_json::Value;

use crate::fixture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int,
    Str,
    List,
    Other,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub kind: Kind,
    /// Allowed values when the schema constrains the field to an enum.
    pub enum_values: Vec<String>,
    /// A non-null schema default, rendered as a string.
    pub default: Option<String>,
}

/// One tool: its field set, the fields its schema requires, and its `mode` enum (empty for a verb
/// tool).
#[derive(Debug, Clone)]
pub struct Domain {
    pub fields: BTreeMap<String, Field>,
    pub required: BTreeSet<String>,
    pub modes: Vec<String>,
}

impl Domain {
    /// The exact accepted/required sets of a verb tool; `None` for a mode-dispatched domain tool,
    /// whose flat schema cannot attribute fields to modes.
    pub fn exact_params(&self) -> Option<ModeParams> {
        self.modes.is_empty().then(|| ModeParams {
            accepted: self.fields.keys().cloned().collect(),
            required: self.required.clone(),
        })
    }
}

fn kind_of(schema: &Value) -> Kind {
    let types: Vec<&str> = match &schema["type"] {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let has = |t: &str| types.contains(&t);
    if has("boolean") {
        Kind::Bool
    } else if has("array") {
        Kind::List
    } else if has("integer") || has("number") {
        Kind::Int
    } else if has("string") {
        Kind::Str
    } else {
        Kind::Other
    }
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Every advertised tool's schema, parsed.
pub fn domains() -> BTreeMap<String, Domain> {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Store::open_for_query(tmp.path(), VIEW_WORKING).expect("open store");
    let git_cache = GitCache::open(&store.basemind_dir, 16, false).expect("open git cache");
    let server = BasemindServer::new_oneshot(
        store,
        tmp.path().to_path_buf(),
        std::sync::Arc::new(basemind::config::default_for_root(tmp.path())),
        None,
        std::sync::Arc::new(git_cache),
    );
    server
        .tool_input_schemas()
        .into_iter()
        .map(|(name, schema)| {
            let props = schema["properties"].as_object().cloned().unwrap_or_default();
            let modes = props.get("mode").map(|m| string_list(&m["enum"])).unwrap_or_default();
            let required = string_list(&schema["required"])
                .into_iter()
                .filter(|field| field != "mode")
                .collect();
            let fields = props
                .iter()
                .filter(|(k, _)| k.as_str() != "mode")
                .map(|(k, v)| {
                    let default = v.get("default").filter(|d| !d.is_null()).map(|d| match d {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                    (
                        k.clone(),
                        Field {
                            kind: kind_of(v),
                            enum_values: string_list(&v["enum"]),
                            default,
                        },
                    )
                })
                .collect();
            (
                name,
                Domain {
                    fields,
                    required,
                    modes,
                },
            )
        })
        .collect()
}

/// What one operation accepts and requires.
#[derive(Debug, Clone)]
pub struct ModeParams {
    pub accepted: BTreeSet<String>,
    pub required: BTreeSet<String>,
}

/// A live in-process MCP client/server pair over a freshly scanned fixture repo.
pub struct Session {
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    pub repo: tempfile::TempDir,
}

impl Session {
    pub async fn start() -> Self {
        let repo = fixture::scanned_repo();
        let transport = basemind::mcp::serve_in_memory(repo.path(), VIEW_WORKING)
            .await
            .expect("serve in memory");
        let service = ().serve(transport).await.expect("mcp handshake");
        Self { service, repo }
    }

    /// End the session and hand back the repo. The server is a writer session holding the workspace
    /// store lock; a CLI command that needs the daemon to open that store (memory, the keyword
    /// lane) can only run once it is gone.
    pub async fn close(self) -> tempfile::TempDir {
        let _ = self.service.cancel().await;
        self.repo
    }

    /// Call `tool` with `args`; the tool's JSON payload on success, the error text on failure.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        let arguments = args.as_object().expect("object args").clone();
        let result = self
            .service
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
            .await
            .map_err(|e| e.to_string())?;
        let text = result
            .content
            .iter()
            .find_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if result.is_error == Some(true) {
            return Err(text);
        }
        basemind::wire::decode(&text).map_err(|e| format!("unreadable payload ({e}): {text}"))
    }
}
