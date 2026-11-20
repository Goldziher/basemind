//! Types of `code_refs`: its arguments, the per-relation arguments its bodies take, and the answer
//! of each relation.

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use crate::engine::cursor::Cursor;
use crate::engine::scope::ChangeScope;
use crate::path::RelPath;

/// Arguments of `code_refs`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeRefsParams {
    /// Name called, implemented or defined; for `importers`, whole segments of the import path.
    #[serde(default)]
    pub name: Option<String>,
    /// What to list (default `callers`). `definition` takes no `limit`, `cursor` or `max_tokens`.
    #[serde(default)]
    #[schemars(extend("enum" = ["callers", "implementations", "importers", "definition", "cycles"]))]
    pub relation: Option<String>,
    /// `callers`: the file defining `name`, to pin it. `definition`: the reference's file (required).
    /// `importers`/`cycles`: that file (no `name`).
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Needs `path`. `callers`: a 1-based line inside the definition meant. `definition`: the
    /// reference's line (required).
    #[serde(default)]
    pub line: Option<u32>,
    /// `definition` only: 1-based column (default: where `name` is on the line).
    #[serde(default)]
    pub column: Option<u32>,
    /// `importers`+`path`: hops (default 1, max 5); `deeper`: more beyond.
    #[serde(default)]
    pub depth: Option<u32>,
    /// `exact` (default): the name or a path ending in it; `substring`: names holding it. Not with
    /// `importers`/`definition`.
    #[serde(default, rename = "match")]
    #[schemars(extend("enum" = ["exact", "substring"]))]
    pub match_mode: Option<String>,
    /// Default 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// `callers` only: keep rows of these resolutions, comma-separated.
    #[serde(default)]
    pub resolution: Option<String>,
    /// `callers` only: files changed since rev (empty: HEAD) or in A..B; +importers adds importers.
    #[serde(default)]
    pub changed: Option<String>,
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
    /// The `resolution` values a row must have to be listed; every row when `None`.
    #[serde(skip)]
    #[schemars(skip)]
    pub resolution: Option<ResolutionFilter>,
    /// The files a `changed` scope holds: call sites elsewhere are not listed. Every file when
    /// `None`.
    #[serde(skip)]
    #[schemars(skip)]
    pub scope: Option<ChangeScope>,
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
    /// The `resolution` values a row must have to be listed; every row when `None`.
    #[serde(skip)]
    #[schemars(skip)]
    pub resolution: Option<ResolutionFilter>,
    /// The files a `changed` scope holds: call sites elsewhere are not listed. Every file when
    /// `None`.
    #[serde(skip)]
    #[schemars(skip)]
    pub scope: Option<ChangeScope>,
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
    /// Importing files, across every page. Written only beside a `next_cursor`: a page without one
    /// lists every importer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub paths: Vec<RelPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

/// Answer of `code_refs` `importers` with `path`: the files that reach it over the resolved
/// file-level import graph (ADR-0047 decision 5), nearest first.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct GraphImportersResponse {
    /// Importing files within `depth`, across every page. Written only beside a `next_cursor`: a
    /// page without one lists every importer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// This page's importers by their import distance from the file (`"1"`: direct importers),
    /// each list in path order.
    pub by_depth: std::collections::BTreeMap<u8, Vec<RelPath>>,
    /// How many importers on this page the file also reaches: they share an import cycle with it,
    /// which `relation: "cycles"` with the same `path` lists. Absent when none.
    #[serde(default, skip_serializing_if = "super::is_zero")]
    pub in_cycle: usize,
    /// Files import the file from beyond `depth`: a larger `depth` (up to 5) lists them. Absent
    /// when none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deeper: bool,
    /// The walk stopped at its node cap, so files may lie beyond it. A walk that stops at the
    /// `depth` asked for is not truncated: `deeper` says whether more lie past it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `scan_cap` whenever `truncated` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<&'static str>,
    /// What the graph cannot see: a language with no import resolver, or a resolve pass still
    /// running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}

/// One import cycle of `code_refs` `cycles`: a strongly connected component of the import graph.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ImportCycle {
    /// Every file of the cycle. Listed along the loop (each imports the next, the last the first)
    /// when one shortest loop runs through all of them, else in path order.
    pub files: Vec<RelPath>,
    /// One shortest loop from the first file, set only when it leaves some of `files` out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<RelPath>,
}

/// Answer of `code_refs` `cycles`: the import cycles, smallest first, or with `path` the one
/// holding that file.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct CyclesResponse {
    /// Cycles across every page.
    pub total: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub cycles: Vec<ImportCycle>,
    /// What a cycle of a language means, for each language a listed cycle has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<&'static str>,
    /// What the graph cannot see: languages with no import resolver, or a resolve pass still
    /// running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
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
    /// How the site was resolved ([`Resolution`]).
    pub resolution: Resolution,
    /// `resolution` is the page's `resolution_default`, so the row leaves it out. Set by
    /// [`crate::engine::refs::resolution::finish_page`].
    #[schemars(skip)]
    pub elide_resolution: bool,
    /// Another call site of the same callee is on this line of this file, in the whole result and
    /// not only this page, so the line alone does not tell them apart and `column` is written. Set
    /// by [`crate::engine::refs::calls_scan::mark_shared_lines`].
    #[schemars(skip)]
    pub shared_line: bool,
    /// The file changed since it was indexed, so `line` is where the call was when it was scanned.
    pub stale: bool,
    /// A `path_match` HTTP call that several routes match equally well (ADR-0048).
    pub ambiguous: bool,
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
        if !self.elide_resolution {
            map.serialize_entry("resolution", &self.resolution)?;
        }
        if self.ambiguous {
            map.serialize_entry("ambiguous", &true)?;
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

/// How a `code_refs` call site was resolved (ADR-0047 decision 6), computed at query time from the
/// resolution facts already indexed. The order is the order `resolution_counts` lists them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Resolution {
    /// Binds to a definition in the same file.
    SameFile,
    /// Binds through an import to a definition in another file.
    Imported,
    /// Binds through scope resolution to a local binding that is not an indexed definition: a
    /// variable, a parameter, or an import whose target is not indexed.
    ScopeResolved,
    /// Checked against the pinned definition; binds to another one.
    Elsewhere,
    /// No resolution facts for the site: matched by name alone.
    NameOnly,
    /// No resolution facts for the site, and the index defines no symbol of the name.
    External,
    /// An HTTP client call linked to a route by its method and normalized path (ADR-0048).
    PathMatch,
}

impl Resolution {
    /// Every value, in [`Resolution`] order.
    pub(crate) const ALL: [Self; 7] = [
        Self::SameFile,
        Self::Imported,
        Self::ScopeResolved,
        Self::Elsewhere,
        Self::NameOnly,
        Self::External,
        Self::PathMatch,
    ];

    /// The value as it is written.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SameFile => "same_file",
            Self::Imported => "imported",
            Self::ScopeResolved => "scope_resolved",
            Self::Elsewhere => "elsewhere",
            Self::NameOnly => "name_only",
            Self::External => "external",
            Self::PathMatch => "path_match",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The `resolution` filter of `code_refs`: the values a row must have to be listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolutionFilter(u8);

impl ResolutionFilter {
    /// Parse a comma-separated list of [`Resolution`] values; the error names the value refused and
    /// the accepted ones.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let mut bits = 0u8;
        for value in raw.split(',').map(str::trim) {
            let Some(resolution) = Resolution::ALL.into_iter().find(|r| r.as_str() == value) else {
                let accepted: Vec<&str> = Resolution::ALL.iter().map(|r| r.as_str()).collect();
                return Err(format!(
                    "unknown resolution `{value}`; expected one or more of {}, comma-separated",
                    accepted.join(", ")
                ));
            };
            bits |= resolution.bit();
        }
        Ok(Self(bits))
    }

    /// Whether a row of `resolution` is listed.
    pub(crate) fn admits(self, resolution: Resolution) -> bool {
        self.0 & resolution.bit() != 0
    }
}

/// Rows per [`Resolution`] value on a page, before the `resolution` filter.
pub(crate) type ResolutionCounts = std::collections::BTreeMap<Resolution, u32>;

/// Whether a page's `resolution_default` goes unwritten: there is none (no rows), or it is
/// `name_only`, which a page without the field means. A page in a language with no resolution
/// engine is all `name_only`, so it pays nothing for the grades.
fn default_is_implied(default: &Option<Resolution>) -> bool {
    matches!(default, None | Some(Resolution::NameOnly))
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct FindReferencesResponse {
    /// The most common `resolution` on the page; a row leaves its own out when it is this one.
    /// Unwritten when it is `name_only`.
    #[serde(skip_serializing_if = "default_is_implied")]
    pub resolution_default: Option<Resolution>,
    /// Rows per `resolution` on the page, before the filter; absent when every row is the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_counts: Option<ResolutionCounts>,
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
    /// Files the `changed` scope holds, when one was passed: `0` is a change that touched nothing,
    /// told apart from a change with no call site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_files: Option<usize>,
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
    /// Why no hit was checked against the definition: `unsupported for <language>`, `no resolution
    /// facts for this file`, `no resolution facts for this definition` (the file has facts and none
    /// names it), or that the index's resolve pass has not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,
    /// As on a name scan: the most common `resolution` on the page, unwritten when `name_only`.
    #[serde(skip_serializing_if = "default_is_implied")]
    pub resolution_default: Option<Resolution>,
    /// As on a name scan: rows per `resolution` on the page, before the filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_counts: Option<ResolutionCounts>,
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
    /// Files the `changed` scope holds, when one was passed: `0` is a change that touched nothing,
    /// told apart from a change with no call site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_files: Option<usize>,
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
    pub resolution_note: Option<String>,
    /// `path_match` when the reference is an HTTP call and `definition` the route its method and
    /// path match (ADR-0048).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    /// The routes an HTTP call matches equally well, when there are several: `definition` is then
    /// absent, since the path alone cannot tell which service answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ambiguous: Vec<DefinitionLocation>,
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
    /// The file changed since it was indexed, so `line` is where the declaration was when it was
    /// scanned.
    pub stale: bool,
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
        if self.stale {
            map.serialize_entry("stale", &true)?;
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
    GraphImporters(GraphImportersResponse),
    Cycles(CyclesResponse),
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
            stale: false,
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
            resolution: Resolution::Elsewhere,
            elide_resolution: false,
            shared_line: false,
            stale: false,
            ambiguous: false,
        };
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            json!({ "line": 9, "callee": "f", "resolution": "elsewhere" })
        );
        assert_eq!(
            serde_json::to_value(call.named("f")).unwrap(),
            json!({ "line": 9, "resolution": "elsewhere" }),
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
            stale: false,
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
            resolution: Resolution::NameOnly,
            elide_resolution: true,
            shared_line: false,
            stale: false,
            ambiguous: false,
        };
        assert_eq!(serde_json::to_value(&call).unwrap(), json!({ "callee": "f" }));
    }

    #[test]
    fn the_resolution_filter_takes_a_comma_list_and_names_a_bad_value() {
        let filter = ResolutionFilter::parse("same_file, imported").unwrap();
        assert!(filter.admits(Resolution::SameFile) && filter.admits(Resolution::Imported));
        assert!(!filter.admits(Resolution::NameOnly));
        let error = ResolutionFilter::parse("same_file,proven").unwrap_err();
        assert!(
            error.contains("`proven`") && error.contains("scope_resolved"),
            "{error}"
        );
        assert!(ResolutionFilter::parse("").is_err(), "an empty list admits nothing");
    }

    #[test]
    fn resolution_counts_are_written_by_value_name() {
        let counts = ResolutionCounts::from([(Resolution::NameOnly, 3), (Resolution::SameFile, 1)]);
        assert_eq!(
            serde_json::to_value(&counts).unwrap(),
            json!({ "same_file": 1, "name_only": 3 })
        );
    }
}
