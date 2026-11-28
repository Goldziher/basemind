//! Binds a pagination cursor to the query that issued it.
//!
//! A cursor is an offset (or a resume key) plus, at most, the generation of the index it was cut
//! from. Nothing in it says WHICH list it is an offset into, so a `code_outline` cursor for one file
//! was accepted by `code_outline` for another, a `code_search` cursor by `docs_search`, a grep
//! cursor by a different pattern: each returned a page of the wrong list with no sign of it.
//!
//! [`bound_value`] wraps every paginated tool body. It hashes the tool name and the arguments that
//! select the list (everything but `limit`, `cursor` and `max_tokens`), prefixes the body's
//! `next_cursor` with that hash, and on the way in refuses a cursor whose prefix is not this query's.
//! It also makes the stale case uniform: a body that reports `cursor_invalidated` (the index or HEAD
//! moved since the cursor was issued) is turned into the same kind of error as a foreign cursor,
//! where some bodies used to return an empty page that read as "no more results".
//!
//! Both adapters (the MCP server and the `basemind` commands) page through here, so a cursor one
//! hands out the other accepts.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::cursor::{Cursor, reject_zero_limit, stale_cursor};
use super::error::EngineError;

/// The arguments of one verb tool: the wire name the error messages cite.
pub trait ToolArgs: DeserializeOwned + schemars::JsonSchema + 'static {
    /// The MCP tool name, e.g. `code_grep`.
    const TOOL: &'static str;
}

/// Arguments that page a list without changing which list it is.
const PAGING_KEYS: [&str; 3] = ["limit", "cursor", "max_tokens"];

/// Separates the query hash from the tool's own cursor. Not in the base64url alphabet.
const SEPARATOR: char = '.';

/// The arguments of a paginated verb tool.
pub trait Paged: ToolArgs + Serialize {
    /// `(field, value)` for each string argument whose omission the tool reads as that value.
    /// Passing the default explicitly selects the same list as leaving it out, so the two must
    /// bind a cursor alike. Each entry mirrors the default applied in the tool's body.
    const DEFAULTS: &'static [(&'static str, &'static str)] = &[];

    /// Arguments where `false` selects a different list from omitting the argument, so it binds a
    /// cursor of its own instead of being read as absent.
    const FALSE_SELECTS: &'static [&'static str] = &[];

    /// The `cursor` argument.
    fn cursor_mut(&mut self) -> &mut Option<Cursor>;

    /// The `limit` argument.
    fn limit(&self) -> Option<u32>;
}

macro_rules! paged {
    ($($params:ty $(=> [$(($field:literal, $default:literal)),+ $(,)?])? $(; false_selects [$($selects:literal),+ $(,)?])?),+ $(,)?) => {
        $(impl Paged for $params {
            $(const DEFAULTS: &'static [(&'static str, &'static str)] = &[$(($field, $default)),+];)?
            $(const FALSE_SELECTS: &'static [&'static str] = &[$($selects),+];)?

            fn cursor_mut(&mut self) -> &mut Option<Cursor> {
                &mut self.cursor
            }

            fn limit(&self) -> Option<u32> {
                self.limit
            }
        })+
    };
}

paged!(
    super::types::code::CodeOutlineParams,
    super::types::code::CodeSymbolsParams,
    super::types::grep::CodeGrepParams,
    super::types::search::CodeSearchParams => [("lane", "hybrid")],
    super::types::refs::CodeRefsParams => [("relation", "callers"), ("match", "exact")],
    super::types::files::FindFilesToolParams => [("indexed", "any")]; false_selects ["indexed"],
    super::types::search::DocsSearchParams,
    super::types::git::GitLogParams,
    super::types::git::GitBlameParams => [("rev", "HEAD")],
    super::types::memory::MemorySearchToolParams => [("visibility", "group")],
);

fn foreign_cursor(tool: &str) -> EngineError {
    EngineError::invalid_argument(format!(
        "`{tool}`: this cursor belongs to a different query. A cursor continues only the call that \
         returned it, with the same arguments apart from `limit` and `max_tokens`. Call again without \
         `cursor`."
    ))
}

/// FNV-1a over `bytes`, continuing from `hash`. Stable across builds and platforms, which a cursor
/// handed to another process (the CLI, a restarted server) needs; `ahash` is seeded per process.
fn fnv1a(mut hash: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Identity of the list a call pages through: the tool and every argument that selects it. An
/// absent, `null` or `false` argument, or one holding its default ([`Paged::DEFAULTS`]), is the
/// same query as one that omits it, except an argument in [`Paged::FALSE_SELECTS`], where `false`
/// is its own selection.
fn query_id<T: Paged>(params: &T) -> u32 {
    let mut hash = fnv1a(0x811c_9dc5, T::TOOL.as_bytes());
    let Ok(Value::Object(arguments)) = serde_json::to_value(params) else {
        return hash;
    };
    for (key, value) in &arguments {
        let is_default = || {
            T::DEFAULTS
                .iter()
                .any(|(field, default)| field == key && value.as_str() == Some(default))
        };
        let is_omitted_false = || *value == Value::Bool(false) && !T::FALSE_SELECTS.contains(&key.as_str());
        if PAGING_KEYS.contains(&key.as_str()) || value.is_null() || is_omitted_false() || is_default() {
            continue;
        }
        hash = fnv1a(hash, &[0]);
        hash = fnv1a(hash, key.as_bytes());
        hash = fnv1a(hash, &[0]);
        hash = fnv1a(hash, value.to_string().as_bytes());
    }
    hash
}

fn bind(cursor: &str, id: u32) -> String {
    format!("{id:08x}{SEPARATOR}{cursor}")
}

/// The tool's own cursor inside a bound one, when the binding is this query's.
fn unbind(tool: &str, cursor: &Cursor, id: u32) -> Result<Cursor, EngineError> {
    let (prefix, inner) = cursor.0.split_once(SEPARATOR).ok_or_else(|| foreign_cursor(tool))?;
    if u32::from_str_radix(prefix, 16) != Ok(id) || prefix.len() != 8 {
        return Err(foreign_cursor(tool));
    }
    Ok(Cursor(inner.to_string()))
}

/// Run a paginated tool body with its cursor bound to the query, and return its answer as JSON.
///
/// Every tool that takes `limit` pages through here, so this is also where `limit: 0` is refused,
/// once and in the same words for all of them.
pub fn bound_value<T, R, F>(mut params: T, body: F) -> Result<Value, EngineError>
where
    T: Paged,
    R: Serialize,
    F: FnOnce(T) -> Result<R, EngineError>,
{
    let id = unbind_params(&mut params)?;
    let answer = body(params)?;
    let value =
        serde_json::to_value(&answer).map_err(|error| EngineError::internal(format!("serialize response: {error}")))?;
    rebind(T::TOOL, value, id)
}

/// Refuse `limit: 0`, replace a bound cursor with the tool's own one, and return the query's id.
fn unbind_params<T: Paged>(params: &mut T) -> Result<u32, EngineError> {
    reject_zero_limit(T::TOOL, params.limit())?;
    let id = query_id(&*params);
    let slot = params.cursor_mut();
    if let Some(cursor) = slot.as_ref() {
        *slot = Some(unbind(T::TOOL, cursor, id)?);
    }
    Ok(id)
}

/// Bind the `next_cursor` of a body's response, and turn a `cursor_invalidated` page into the
/// stale-cursor error.
fn rebind(tool: &str, answer: Value, id: u32) -> Result<Value, EngineError> {
    let Value::Object(mut body) = answer else {
        return Ok(answer);
    };
    if body.remove("cursor_invalidated") == Some(Value::Bool(true)) {
        return Err(stale_cursor(tool));
    }
    if let Some(Value::String(cursor)) = body.get_mut("next_cursor") {
        *cursor = bind(cursor, id);
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;
