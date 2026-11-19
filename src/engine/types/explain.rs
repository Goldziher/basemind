//! Arguments and answer of `basemind code explain` (#120): one briefing on a symbol or a file,
//! composed from the outline, `code_refs`, the import graph and git history. A CLI-only command
//! (ADR-0047 decision 1) until an `explain` eval run earns it a tool, so these types are no schema.

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use super::LifecycleNotice;
use crate::path::RelPath;

/// Arguments of `code explain`.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExplainParams {
    /// A symbol name, an indexed file, or `path:line` (the innermost definition holding that line).
    pub target: String,
    /// Comma-separated sections to report; all when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<String>,
    /// Rows per list section (default 20, at most 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Leave the symbol's body out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_code: bool,
    /// Token budget of the whole answer (default 6000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// The definition a symbol briefing is about.
#[derive(Debug, Serialize)]
pub(crate) struct ExplainDefinition {
    pub path: RelPath,
    pub name: String,
    pub kind: &'static str,
    /// 1-based first and last line.
    pub line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The doc comment directly above the definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// The file changed since the scan; the definition is read from the working tree.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

/// One top-level definition of a file briefing.
#[derive(Debug, Serialize)]
pub(crate) struct ExplainOutlineRow {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
}

/// One commit of a briefing's history.
#[derive(Debug, Serialize)]
pub(crate) struct ExplainCommit {
    pub sha: String,
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    pub summary: String,
}

/// An author of the recent commits touching a file, with how many of them they wrote.
#[derive(Debug, Serialize)]
pub(crate) struct ExplainOwner {
    pub author: String,
    pub commits: u32,
}

/// A section the answer cut, and the call that returns the rest of it.
#[derive(Debug, Serialize)]
pub(crate) struct TruncatedSection {
    pub section: &'static str,
    /// Rows listed, and rows there are when known.
    pub shown: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// `limit` (the per-section row limit) or `budget` (the whole answer's token budget).
    pub reason: &'static str,
    /// The tool call that continues the section, as `tool {"arg": ...}`.
    pub next: String,
}

/// Answer of `code explain`. A section is absent when it was not asked for or does not apply to
/// the target kind.
#[derive(Debug, Default, Serialize)]
pub(crate) struct ExplainResponse {
    /// `symbol` or `file`.
    pub target: &'static str,
    /// Symbol: the definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<ExplainDefinition>,
    /// Symbol: other definitions of the name, as `path:line`, when it named several and one callable
    /// was chosen.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_defined: Vec<String>,
    /// Symbol: its source, when it spans at most 60 lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Symbol: call sites by file, each row with its line and, when it differs from
    /// `callers_resolution_default`, its resolution (as `code_refs` callers answers them).
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "grouped")]
    pub callers: Option<Vec<(String, Value)>>,
    /// Symbol: call sites in all, `callers` holding the first `limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_total: Option<usize>,
    /// Symbol: call sites proven to bind to this definition; absent when resolution did not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_resolved: Option<u64>,
    /// Symbol: the resolution of a caller row that writes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callers_resolution_default: Option<Value>,
    /// Symbol: names the body calls, in order of first call, without the symbol itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callees: Option<Vec<String>>,
    /// File: the file briefed, its language, lines and definitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<usize>,
    /// File: its definitions outside function bodies, in source order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline: Option<Vec<ExplainOutlineRow>>,
    /// File: the files it imports (runtime imports, one hop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imports: Option<Vec<RelPath>>,
    /// The files importing the file (of the definition, for a symbol): runtime imports, one hop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importers: Option<Vec<RelPath>>,
    /// File: the file's row of `git hotspots` over the last 90 days (empty `signals` when it is
    /// no hotspot).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotspot: Option<Value>,
    /// Recent commits that changed the symbol's body, or touched the file; newest first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<ExplainCommit>>,
    /// File: the authors of its last 20 commits, most commits first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owners: Option<Vec<ExplainOwner>>,
    /// Sections cut by `limit` or the token budget, each with the call that continues it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncated_sections: Vec<TruncatedSection>,
    /// What a section could not see: no git repository, no import resolver for the language, a
    /// resolution note.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    pub elapsed_us: u64,
}

/// `(path, row)` pairs written as `{"<path>": [row, ...]}`, files in order of their first row.
fn grouped<S: Serializer>(rows: &Option<Vec<(String, Value)>>, serializer: S) -> Result<S::Ok, S::Error> {
    let mut map = serde_json::Map::new();
    for (path, row) in rows.iter().flatten() {
        if let Value::Array(list) = map.entry(path.clone()).or_insert_with(|| Value::Array(Vec::new())) {
            list.push(row.clone());
        }
    }
    map.serialize(serializer)
}
