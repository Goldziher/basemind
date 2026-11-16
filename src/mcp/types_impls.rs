//! Param and response types for the `find_implementations` MCP tool.
//!
//! Split out of `types.rs` to keep that file under the 1000-line cap.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use crate::path::RelPath;

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
    #[serde(serialize_with = "super::by_path::serialize")]
    pub hits: Vec<ImplementationHit>,
    /// Opaque cursor to pass back on the next call when more results are available.
    /// Stable across rescans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// Lifecycle notice when the server isn't fully ready (warming/building/rescanning); absent when
    /// ready. Lets a caller tell "index still loading — retry" from a genuine empty result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<super::types::LifecycleNotice>,
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
    pub(super) fn named(mut self, name: &str) -> Self {
        if self.trait_name == name {
            self.trait_name.clear();
        }
        self
    }
}

/// Written by hand so an implementation with no recorded position omits `line` instead of
/// reporting line 0, and so a trait that is the name asked for is not repeated on every row.
impl super::by_path::Located for ImplementationHit {
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
        super::by_path::Located::serialize_row(self, serializer, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types::ReferenceHit;
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
        };
        assert_eq!(serde_json::to_value(&call).unwrap(), json!({ "callee": "f" }));
    }
}
