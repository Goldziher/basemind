//! Types of `code_refs`: its arguments, the per-relation arguments its bodies take, and the answer
//! of each relation.

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use crate::engine::cursor::Cursor;
use crate::path::RelPath;

/// Arguments of `code_refs`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeRefsParams {
    /// Name called, implemented or defined; for `importers`, whole segments of the import path.
    pub name: String,
    /// What to list (default `callers`).
    #[serde(default)]
    #[schemars(extend("enum" = ["callers", "implementations", "importers", "definition"]))]
    pub relation: Option<String>,
    /// `callers`: file defining `name`, pinning that definition. `definition`: file holding the
    /// reference (required). Not with `implementations` or `importers`.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Needs `path`. `callers`: 1-based line inside the definition meant, when several share the
    /// name. `definition`: line of the reference (required).
    #[serde(default)]
    pub line: Option<u32>,
    /// `definition` only: 1-based column (default: where `name` occurs on the line).
    #[serde(default)]
    pub column: Option<u32>,
    /// Name matching (default `exact`). Not with `importers` or `definition`.
    #[serde(default, rename = "match")]
    #[schemars(extend("enum" = ["exact", "substring"]))]
    pub match_mode: Option<String>,
    /// Max hits (default 100). Not with `definition`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page. Not with `definition`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Token budget for the list (default 6000). Not with `definition`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    /// The callee identifier to look up. Substring match — case-sensitive, no scope
    /// resolution; both `Foo::bar()` and `bar()` register as callee `"bar"`. Use with
    /// caution on common names like `new` or `get`.
    pub name: String,
    /// Match callee identifiers containing `name` instead of only those equal to it.
    #[serde(default)]
    pub substring: bool,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindCallersParams {
    /// Repository-relative path of the definition file.
    pub path: RelPath,
    /// Name of the definition.
    pub name: String,
    /// Match callee identifiers containing `name` instead of only those equal to it.
    #[serde(default)]
    pub substring: bool,
    /// Optional kind filter for resolving the definition (function/method/class/...).
    #[serde(default)]
    pub kind: Option<String>,
    /// 1-based line inside the definition meant, when several in the file share the name.
    #[serde(default)]
    pub line: Option<u32>,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GotoDefinitionParams {
    /// Repository-relative path of the file holding the reference.
    pub path: RelPath,
    /// 1-based line of the reference identifier.
    pub line: u32,
    /// 0-based byte column of the reference within the line. Any byte inside the identifier
    /// resolves for span-aware engines (oxc JS/TS); the tree-sitter `locals` fallback matches
    /// only the identifier's start byte. `None` takes the first whole-identifier occurrence of
    /// `name` on the line.
    #[serde(default)]
    pub column: Option<u32>,
    /// The referenced name, looked for on the line when `column` is absent.
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct DependentsResponse {
    /// Importing files, across every page.
    pub total: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub paths: Vec<RelPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

#[derive(Debug, schemars::JsonSchema)]
pub(crate) struct ReferenceHit {
    /// The file of the call site: the key the hit is listed under.
    pub path: RelPath,
    /// 1-based line of the call site; `0` when the index holds no position for it.
    pub line: u32,
    /// Byte column of the call site: 0-based here, written 1-based, and only when
    /// [`shared_line`](Self::shared_line) is set.
    pub column: u32,
    /// The name called. Written only when it is not the name asked for (a `substring` match, or an
    /// alias the definition is called by); [`ReferenceHit::named`] clears it otherwise.
    pub callee: String,
    /// With a pinned definition: `true` when scope / import resolution proved this site binds to
    /// it, `false` when resolution checked the site and did not. Absent when nothing was checked:
    /// no definition was pinned, resolution has no facts about the definition (`resolution` on the
    /// response says why), or the site's file lies past the probe cap.
    pub resolved: Option<bool>,
    /// Another call site of the same callee is on this line of this file, in the whole result and
    /// not only this page, so the line alone does not tell them apart and `column` is written. Set
    /// by [`crate::engine::refs::calls_scan::mark_shared_lines`].
    #[schemars(skip)]
    pub shared_line: bool,
    /// The file changed since it was indexed, so `line` is where the call was when it was scanned.
    pub stale: bool,
}

impl ReferenceHit {
    /// This hit as listed for a query of `name`: the callee is left out when it is `name`.
    pub(crate) fn named(mut self, name: &str) -> Self {
        if self.callee == name {
            self.callee.clear();
        }
        self
    }
}

/// Written by hand so a call site with no recorded position omits `line` instead of reporting line
/// 0, and so a callee that is the name asked for is not repeated on every row.
impl crate::engine::by_path::Located for ReferenceHit {
    fn located(&self) -> &RelPath {
        &self.path
    }

    fn writes_column(&self) -> Option<bool> {
        Some(self.shared_line)
    }

    fn line(&self) -> u32 {
        self.line
    }

    fn serialize_row<S: serde::Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        if self.line != 0 {
            map.serialize_entry("line", &self.line)?;
            if column {
                map.serialize_entry("column", &self.column.saturating_add(1))?;
            }
        }
        if !self.callee.is_empty() {
            map.serialize_entry("callee", &self.callee)?;
        }
        if let Some(resolved) = self.resolved {
            map.serialize_entry("resolved", &resolved)?;
        }
        if self.stale {
            map.serialize_entry("stale", &true)?;
        }
        map.end()
    }
}

/// A call site on its own, as the token budget costs it.
impl Serialize for ReferenceHit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::engine::by_path::Located::serialize_row(self, serializer, false)
    }
}

/// The `resolution` of a `code_refs` name scan: hits are matched by name and nothing binds them to
/// one definition.
pub(crate) const NAME_ONLY: &str = "name only: pass `path` to check each hit against that file's definition";

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct FindReferencesResponse {
    /// Always [`NAME_ONLY`]: no hit was checked against a definition.
    pub resolution: &'static str,
    /// Matches across every page. A lower bound when `truncated`; an upper bound when a daemon that
    /// predates exact matching answered, whose substring hits are dropped page by page.
    pub total: u32,
    /// True when the scan stopped at its cap before the end of the matches: `total` is then a
    /// lower bound, and `next_cursor` resumes the scan.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `scan_cap` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The call sites under the file each is in.
    #[serde(serialize_with = "crate::engine::by_path::serialize")]
    pub hits: Vec<ReferenceHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct FindCallersResponse {
    /// The definition the callers were pinned to.
    pub definition: DefinitionView,
    /// Why no hit carries `resolved`: `unsupported for <language>`, `no resolution facts for this
    /// file`, `no resolution facts for this definition` (the file has facts and none names it), or
    /// that the index's resolve pass has not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Hits proven to bind to `definition`. Absent, not 0, when resolution did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_total: Option<u32>,
    /// Matches across every page. A lower bound when `truncated`; an upper bound when a daemon that
    /// predates exact matching answered, whose substring hits are dropped page by page.
    pub total: u32,
    /// As on a `code_refs` name scan: the scan stopped at its cap, so `total` is a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The call sites under the file each is in.
    #[serde(serialize_with = "crate::engine::by_path::serialize")]
    pub hits: Vec<ReferenceHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct DefinitionView {
    pub path: RelPath,
    pub name: String,
    pub kind: &'static str,
    /// 1-based line of the definition.
    pub line: u32,
    /// 1-based column of the definition.
    pub column: u32,
}

/// A resolved definition site returned by `goto_definition`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct DefinitionLocation {
    pub path: RelPath,
    /// 1-based.
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
    /// The definition identifier text, when the engine recorded its span (empty otherwise).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct GotoDefinitionResponse {
    /// Echo of the queried file.
    pub path: RelPath,
    /// Normalized 1-based line / 0-based byte column of the queried position.
    pub line: u32,
    pub column: u32,
    /// The resolved definition, or absent when the position holds no resolved binding
    /// (module-global, unresolved name, or a language without resolution coverage).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<DefinitionLocation>,
    /// Why there is no `definition`, when the file's language has no scope resolution:
    /// `unsupported for <language>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindImplementationsParams {
    /// The trait / interface / base-class name to find implementations of.
    pub trait_name: String,
    /// Match trait names containing `trait_name` instead of only the trait of that name.
    #[serde(default)]
    pub substring: bool,
    /// Optional language filter (e.g. "rust", "typescript"). When set, only matches
    /// from files in that language are returned.
    #[serde(default)]
    pub language: Option<String>,
    /// Cap on results returned. Default 100, max 1000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept in scan order until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true` plus a `next_cursor` to page them.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Resume token returned by the previous call's `next_cursor`. Stable across rescans
    /// because the underlying Fjall keys are content-addressed.
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FindImplementationsResponse {
    /// Matches across every page. A lower bound when `truncated`; an upper bound when a daemon that
    /// predates exact matching answered, whose substring hits are dropped page by page.
    pub total: usize,
    /// True when the scan stopped at its cap before the end of the matches: `total` is then a
    /// lower bound, and `next_cursor` resumes the scan.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `scan_cap` whenever `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// True when a `max_tokens` budget dropped trailing `hits`. Page the rest with `next_cursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// The implementations under the file each is in.
    #[serde(serialize_with = "crate::engine::by_path::serialize")]
    pub hits: Vec<ImplementationHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

#[derive(Debug, schemars::JsonSchema)]
pub struct ImplementationHit {
    /// The file the implementation is in: the key it is listed under.
    pub path: RelPath,
    /// The trait implemented. Written only when it is not the name asked for (a `substring`
    /// match); [`ImplementationHit::named`] clears it otherwise.
    pub trait_name: String,
    pub impl_type: String,
    /// 1-based row of the `impl`/`class`/`extends` declaration, written as `line`; `0` when the
    /// outline holds no record of it.
    pub start_row: u32,
    /// Byte column from the start of the line: 0-based here, written 1-based as `column`, and only
    /// when another implementation is on the same line.
    pub start_col: u32,
}

impl ImplementationHit {
    /// This hit as listed for a query of `name`: the trait is left out when it is `name`.
    pub(crate) fn named(mut self, name: &str) -> Self {
        if self.trait_name == name {
            self.trait_name.clear();
        }
        self
    }
}

/// Written by hand so an implementation with no recorded position omits `line` instead of
/// reporting line 0, and so a trait that is the name asked for is not repeated on every row.
impl crate::engine::by_path::Located for ImplementationHit {
    fn located(&self) -> &RelPath {
        &self.path
    }

    fn line(&self) -> u32 {
        self.start_row
    }

    fn serialize_row<S: serde::Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        if self.start_row != 0 {
            map.serialize_entry("line", &self.start_row)?;
            if column {
                map.serialize_entry("column", &self.start_col.saturating_add(1))?;
            }
        }
        map.serialize_entry("impl_type", &self.impl_type)?;
        if !self.trait_name.is_empty() {
            map.serialize_entry("trait_name", &self.trait_name)?;
        }
        map.end()
    }
}

/// An implementation on its own, as the token budget costs it.
impl Serialize for ImplementationHit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::engine::by_path::Located::serialize_row(self, serializer, false)
    }
}

/// Answer of `code_refs`, by relation: call sites of a name, the callers of one definition, the
/// implementations of a trait, the importers of a module, or the definition a reference binds to.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum CodeRefsResult {
    References(FindReferencesResponse),
    Callers(FindCallersResponse),
    Implementations(FindImplementationsResponse),
    Importers(DependentsResponse),
    Definition(GotoDefinitionResponse),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_hit_writes_its_line_and_leaves_its_file_to_the_group() {
        let implementation = ImplementationHit {
            path: "a.rs".into(),
            trait_name: "T".to_string(),
            impl_type: "S".to_string(),
            start_row: 4,
            start_col: 0,
        };
        assert_eq!(
            serde_json::to_value(&implementation).unwrap(),
            json!({ "line": 4, "impl_type": "S", "trait_name": "T" })
        );
        assert_eq!(
            serde_json::to_value(implementation.named("T")).unwrap(),
            json!({ "line": 4, "impl_type": "S" }),
            "the trait asked for is not repeated"
        );
        let call = ReferenceHit {
            path: "a.rs".into(),
            line: 9,
            column: 2,
            callee: "f".to_string(),
            resolved: Some(false),
            shared_line: false,
            stale: false,
        };
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            json!({ "line": 9, "callee": "f", "resolved": false })
        );
        assert_eq!(
            serde_json::to_value(call.named("f")).unwrap(),
            json!({ "line": 9, "resolved": false }),
            "the callee asked for is not repeated"
        );
    }

    #[test]
    fn a_hit_without_a_position_omits_it_rather_than_reporting_line_zero() {
        let implementation = ImplementationHit {
            path: "a.rs".into(),
            trait_name: "T".to_string(),
            impl_type: "S".to_string(),
            start_row: 0,
            start_col: 0,
        };
        assert_eq!(
            serde_json::to_value(&implementation).unwrap(),
            json!({ "impl_type": "S", "trait_name": "T" })
        );
        let call = ReferenceHit {
            path: "a.rs".into(),
            line: 0,
            column: 0,
            callee: "f".to_string(),
            resolved: None,
            shared_line: false,
            stale: false,
        };
        assert_eq!(serde_json::to_value(&call).unwrap(), json!({ "callee": "f" }));
    }
}
