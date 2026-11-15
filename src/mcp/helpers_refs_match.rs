//! Exact name matching for `code_refs`.
//!
//! The reference and implementation scans (`calls_by_callee`, `implementations_by_trait`, their
//! in-RAM projections and the daemon-forwarded forms) all match a name as a SUBSTRING, so the
//! callers of `file_outline` included every call to `file_outline_l2`. Exact matching is applied
//! here, above the scans: pages are pulled through whichever scan the session routes to and only
//! the hits whose identifier IS the name are kept. Keeping the scans themselves unchanged means a
//! local index, a forwarded daemon read and the in-RAM fallback cannot disagree about what "exact"
//! means.

use rmcp::ErrorData as McpError;

use super::MapCache;
use super::cursor::Cursor;
use super::helpers_calls_scan::CallScanPage;
use super::helpers_impls::ImplScanPage;
use super::index_route::IndexRoute;

/// Hits pulled from the underlying substring scan per round.
const SOURCE_PAGE: usize = 1_000;
/// Rounds before the exact total is reported as partial. Bounds the work on a name that is a
/// substring of a very common one (`get` inside `get_or_insert`, `target`, ...).
const SOURCE_PAGES_MAX: usize = 16;

/// How `code_refs` compares the requested name with an indexed identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Matching {
    Exact,
    Substring,
}

impl Matching {
    pub(super) fn parse(raw: Option<&str>) -> Result<Self, McpError> {
        match raw.unwrap_or("exact") {
            "exact" => Ok(Self::Exact),
            "substring" => Ok(Self::Substring),
            other => Err(McpError::invalid_params(
                format!("`code_refs`: unknown match `{other}`; expected `exact` or `substring`"),
                None,
            )),
        }
    }

    pub(super) fn from_substring(substring: bool) -> Self {
        if substring { Self::Substring } else { Self::Exact }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Substring => "substring",
        }
    }
}

/// Whether `identifier` names `name`: equal to it, or a qualified path whose last segment (or
/// trailing segments, when `name` is itself qualified) is `name`. `Foo::bar` and `self.bar` are
/// calls to `bar`; `bar_baz` and `rebar` are not.
pub(super) fn identifier_is(identifier: &str, name: &str) -> bool {
    let Some(prefix) = identifier.strip_suffix(name) else {
        return false;
    };
    prefix.is_empty() || prefix.ends_with([':', '.', '>', '/', '\\'])
}

/// Whether an implemented trait reference names `name`, ignoring generic arguments:
/// `Iterator<Item = u8>` and `std::iter::Iterator` both implement `Iterator`.
pub(super) fn trait_is(trait_name: &str, name: &str) -> bool {
    let bare = trait_name.split(['<', '(', '[']).next().unwrap_or(trait_name).trim();
    identifier_is(bare, name)
}

/// The call sites of `name`: the routed scan as is for [`Matching::Substring`], filtered to exact
/// identifiers otherwise.
pub(super) async fn scan_calls(
    route: &IndexRoute,
    cache: &MapCache,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<CallScanPage, McpError> {
    if matching == Matching::Substring {
        return route.scan_calls(cache, name, limit, cursor_after).await;
    }
    let mut out = CallScanPage {
        total: 0,
        total_is_partial: false,
        hits: Vec::new(),
        next_cursor: None,
        hit_keys: Vec::new(),
        hit_starts: Vec::new(),
    };
    let mut cursor: Option<Vec<u8>> = cursor_after.map(<[u8]>::to_vec);
    let mut has_more = false;
    let mut exhausted = false;
    let mut source_cursor: Option<Vec<u8>> = None;
    for _ in 0..SOURCE_PAGES_MAX {
        let page = route.scan_calls(cache, name, SOURCE_PAGE, cursor.as_deref()).await?;
        let more_in_source = page.next_cursor.is_some();
        let last_key = page.hit_keys.last().cloned();
        for ((hit, key), start) in page.hits.into_iter().zip(page.hit_keys).zip(page.hit_starts) {
            if !identifier_is(&hit.callee, name) {
                continue;
            }
            out.total += 1;
            if out.hits.len() < limit {
                out.hits.push(hit);
                out.hit_keys.push(key);
                out.hit_starts.push(start);
            } else {
                has_more = true;
            }
        }
        if !more_in_source || last_key.is_none() {
            exhausted = true;
            break;
        }
        source_cursor.clone_from(&last_key);
        cursor = last_key;
    }
    out.total_is_partial = !exhausted;
    // A full page resumes after its last hit; a scan cut short resumes where the source stopped.
    out.next_cursor = if has_more {
        out.hit_keys.last().map(|key| Cursor::encode_fjall(key))
    } else if !exhausted {
        source_cursor.as_deref().map(Cursor::encode_fjall)
    } else {
        None
    };
    Ok(out)
}

/// The implementors of `name`: the routed scan as is for [`Matching::Substring`], filtered to the
/// trait of exactly that name otherwise.
pub(super) async fn scan_impls(
    route: &IndexRoute,
    cache: &MapCache,
    name: &str,
    matching: Matching,
    language: Option<&str>,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<ImplScanPage, McpError> {
    if matching == Matching::Substring {
        return route.scan_impls(cache, name, language, limit, cursor_after).await;
    }
    let mut out = ImplScanPage {
        total: 0,
        total_is_partial: false,
        hits: Vec::new(),
        hit_keys: Vec::new(),
        has_more: false,
    };
    let mut cursor: Option<Vec<u8>> = cursor_after.map(<[u8]>::to_vec);
    let mut exhausted = false;
    for _ in 0..SOURCE_PAGES_MAX {
        let page = route
            .scan_impls(cache, name, language, SOURCE_PAGE, cursor.as_deref())
            .await?;
        let more_in_source = page.has_more;
        let last_key = page.hit_keys.last().cloned();
        for (hit, key) in page.hits.into_iter().zip(page.hit_keys) {
            if !trait_is(&hit.trait_name, name) {
                continue;
            }
            out.total += 1;
            if out.hits.len() < limit {
                out.hits.push(hit);
                out.hit_keys.push(key);
            } else {
                out.has_more = true;
            }
        }
        if !more_in_source || last_key.is_none() {
            exhausted = true;
            break;
        }
        cursor = last_key;
    }
    out.total_is_partial = !exhausted;
    out.has_more |= !exhausted && !out.hit_keys.is_empty();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_matches_only_its_own_name() {
        assert!(identifier_is("file_outline", "file_outline"));
        assert!(!identifier_is("file_outline_l2", "file_outline"));
        assert!(!identifier_is("my_file_outline", "file_outline"));
    }

    #[test]
    fn a_qualified_callee_matches_its_last_segment() {
        assert!(identifier_is("Foo::bar", "bar"));
        assert!(identifier_is("self.bar", "bar"));
        assert!(identifier_is("ptr->bar", "bar"));
        assert!(identifier_is("crate::outline::file_outline", "outline::file_outline"));
        assert!(!identifier_is("Foo::rebar", "bar"));
        assert!(!identifier_is("bar", "Foo::bar"));
    }

    #[test]
    fn a_trait_matches_through_its_generic_arguments_and_path() {
        assert!(trait_is("Iterator", "Iterator"));
        assert!(trait_is("Iterator<Item = u8>", "Iterator"));
        assert!(trait_is("std::iter::Iterator", "Iterator"));
        assert!(!trait_is("DoubleEndedIterator", "Iterator"));
        assert!(!trait_is("IteratorExt", "Iterator"));
    }

    #[test]
    fn matching_parses_its_two_spellings_and_defaults_to_exact() {
        assert_eq!(Matching::parse(None).unwrap(), Matching::Exact);
        assert_eq!(Matching::parse(Some("substring")).unwrap(), Matching::Substring);
        assert!(Matching::parse(Some("prefix")).is_err());
    }
}
