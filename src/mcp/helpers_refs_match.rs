//! Exact name matching for `code_refs`.
//!
//! The reference and implementation scans (`calls_by_callee`, `implementations_by_trait`, their
//! in-RAM projections and the daemon-forwarded forms) walk every identifier that CONTAINS the name,
//! so without a second test the callers of `file_outline` included every call to
//! `file_outline_l2`. [`Matching`] is that test. Each scan applies it on the key, before it
//! resolves a position or counts the hit toward its scan cap, so the local index, a forwarded
//! daemon read and the in-RAM fallback agree on what "exact" means and none of them can spend its
//! cap on substring-only identifiers.

use rmcp::ErrorData as McpError;

/// How `code_refs` compares the requested name with an indexed identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Matching {
    Exact,
    Substring,
}

impl Matching {
    /// Whether a callee identifier that contains `name` is a call to `name` under this mode.
    pub(crate) fn accepts_callee(self, callee: &str, name: &str) -> bool {
        self == Self::Substring || identifier_is(callee, name)
    }

    /// Whether an implemented trait reference that contains `name` names `name` under this mode.
    pub(crate) fn accepts_trait(self, trait_name: &str, name: &str) -> bool {
        self == Self::Substring || trait_is(trait_name, name)
    }

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

    pub(crate) fn from_substring(substring: bool) -> Self {
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
fn identifier_is(identifier: &str, name: &str) -> bool {
    let Some(prefix) = identifier.strip_suffix(name) else {
        return false;
    };
    prefix.is_empty() || prefix.ends_with([':', '.', '>', '/', '\\'])
}

/// Whether an implemented trait reference names `name`, ignoring generic arguments:
/// `Iterator<Item = u8>` and `std::iter::Iterator` both implement `Iterator`.
fn trait_is(trait_name: &str, name: &str) -> bool {
    let bare = trait_name.split(['<', '(', '[']).next().unwrap_or(trait_name).trim();
    identifier_is(bare, name)
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
    fn substring_matching_accepts_whatever_the_scan_found() {
        assert!(Matching::Substring.accepts_callee("file_outline_l2", "file_outline"));
        assert!(!Matching::Exact.accepts_callee("file_outline_l2", "file_outline"));
        assert!(Matching::Substring.accepts_trait("DoubleEndedIterator", "Iterator"));
        assert!(!Matching::Exact.accepts_trait("DoubleEndedIterator", "Iterator"));
    }

    #[test]
    fn matching_parses_its_two_spellings_and_defaults_to_exact() {
        assert_eq!(Matching::parse(None).unwrap(), Matching::Exact);
        assert_eq!(Matching::parse(Some("substring")).unwrap(), Matching::Substring);
        assert!(Matching::parse(Some("prefix")).is_err());
    }
}
