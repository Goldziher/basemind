//! The live MCP surface: tool input schemas, and a per-mode probe of which parameters each mode
//! accepts and requires.
//!
//! basemind publishes ONE flat input schema per domain tool (the Anthropic subset forbids `oneOf`),
//! so the schema alone cannot say which fields belong to which mode. The server's own validator can:
//! it rejects out-of-mode fields up front with ``` `<tool>` mode `<m>` does not accept `a`, `b` ```,
//! and reports missing ones as ``` requires `f` ```. The probe drives those messages through an
//! in-process MCP client, so the "accepted"/"required" sets are what the real validator enforces,
//! not what a doc comment claims.

use std::collections::{BTreeMap, BTreeSet};

use basemind::cli::context::build_server;
use basemind::config::DocumentsCliOverrides;
use basemind::store::VIEW_WORKING;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use serde_json::{Value, json};

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

/// One domain tool: its flat field set and its `mode` enum.
#[derive(Debug, Clone)]
pub struct Domain {
    pub fields: BTreeMap<String, Field>,
    pub modes: Vec<String>,
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
    let server =
        build_server(tmp.path(), VIEW_WORKING, DocumentsCliOverrides::default()).expect("build one-shot server");
    server
        .tool_input_schemas()
        .into_iter()
        .map(|(name, schema)| {
            let props = schema["properties"].as_object().cloned().unwrap_or_default();
            let modes = props.get("mode").map(|m| string_list(&m["enum"])).unwrap_or_default();
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
            (name, Domain { fields, modes })
        })
        .collect()
}

/// What one mode accepts and requires, as enforced by the server's validator.
#[derive(Debug, Clone)]
pub struct ModeParams {
    pub accepted: BTreeSet<String>,
    pub required: BTreeSet<String>,
}

/// Tools whose handlers can reach the network, spawn processes, download models or need the comms
/// broker. They are not probed (probing executes the handler whenever a field is accepted); their
/// parameters are compared domain-wide instead.
const UNPROBED_TOOLS: &[&str] = &["memory", "web", "agents", "workspace", "shell"];
/// Individual modes that open a browser / run the embedding stack when their fields are accepted.
const UNPROBED_MODES: &[(&str, &str)] = &[
    ("code", "semantic"),
    ("code", "chunk"),
    ("graph", "open"),
    ("graph", "display"),
];

pub fn is_probed(tool: &str, mode: &str) -> bool {
    !UNPROBED_TOOLS.contains(&tool) && !UNPROBED_MODES.contains(&(tool, mode))
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
        serde_json::from_str(&text).map_err(|e| format!("non-JSON payload ({e}): {text}"))
    }

    /// Probe which fields `mode` accepts and requires.
    pub async fn probe(&self, tool: &str, mode: &str, domain: &Domain) -> ModeParams {
        let dummy = |name: &str| -> Value {
            let field = &domain.fields[name];
            match field.kind {
                Kind::Bool => json!(true),
                Kind::Int => json!(1),
                Kind::List => json!(["x"]),
                _ => json!(field.enum_values.first().cloned().unwrap_or_else(|| "x".to_string())),
            }
        };
        let with = |names: &[&String]| -> Value {
            let mut args = serde_json::Map::new();
            args.insert("mode".into(), json!(mode));
            for name in names {
                args.insert((*name).clone(), dummy(name));
            }
            Value::Object(args)
        };

        // All fields at once: the validator names every out-of-mode field in one message.
        let all: Vec<&String> = domain.fields.keys().collect();
        let mut rejected = rejected_in(&self.call(tool, with(&all)).await);
        if rejected.is_empty() {
            // Dummies failed to deserialize (or nothing was rejected): ask one field at a time.
            for name in &all {
                if rejected_in(&self.call(tool, with(&[name])).await).contains(*name) {
                    rejected.insert((*name).clone());
                }
            }
        }
        let accepted: BTreeSet<String> = all
            .iter()
            .filter(|n| !rejected.contains(**n))
            .map(|n| (*n).clone())
            .collect();

        // Required: keep supplying whatever the validator says it still requires.
        let mut required = BTreeSet::new();
        for _ in 0..12 {
            let supplied: Vec<&String> = required.iter().collect();
            let Err(message) = self.call(tool, with(&supplied)).await else {
                break;
            };
            let missing: Vec<String> = backticked_after(&message, &["requires ", "missing field "])
                .into_iter()
                .filter(|f| domain.fields.contains_key(f) && !required.contains(f))
                .collect();
            if missing.is_empty() {
                break;
            }
            required.extend(missing);
        }
        ModeParams { accepted, required }
    }
}

/// Field names a `does not accept` error lists.
fn rejected_in(result: &Result<Value, String>) -> BTreeSet<String> {
    match result {
        Err(message) if message.contains("does not accept") => {
            backticked_after(message, &["does not accept "]).into_iter().collect()
        }
        _ => BTreeSet::new(),
    }
}

/// Backtick-quoted identifiers following any of the `markers` (``requires `path` ``, ``does not
/// accept `a`, `b` ``). Takes the run of `` `x` `` tokens joined by `, `/` and `/` or `.
fn backticked_after(message: &str, markers: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for marker in markers {
        let Some(start) = message.find(marker) else { continue };
        let mut rest = &message[start + marker.len()..];
        while let Some(tail) = rest.strip_prefix('`') {
            let Some(end) = tail.find('`') else { break };
            out.push(tail[..end].to_string());
            rest = tail[end + 1..].trim_start_matches([',', ' ']);
            rest = rest
                .strip_prefix("and ")
                .or_else(|| rest.strip_prefix("or "))
                .unwrap_or(rest);
        }
    }
    out
}
