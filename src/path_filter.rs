//! The `path` filter of the search and list tools (`code_grep`, `find_files`, `docs_search`,
//! `code_symbols`, `code_search`): one argument with one matching rule.
//!
//! A value without `*` is a repo-relative prefix, compared byte for byte: `src/mcp/` keeps
//! `src/mcp/tools.rs`, and `src/mc` keeps `src/mcp/tools.rs` and `src/mcp_types.rs` alike. A value
//! holding `*` is a glob over the whole repo-relative path: `*` stays within one segment, `**`
//! crosses `/`, and `?`, `[...]` and `{a,b}` take their glob meaning. Without `*`, those characters
//! are literal, so `app/[id]/` is the prefix of `app/[id]/page.tsx`.

use bstr::ByteSlice;
use globset::{GlobBuilder, GlobMatcher};

/// A parsed `path` filter.
#[derive(Debug, Clone)]
pub enum PathFilter {
    /// A value without `*`: the path must start with it.
    Prefix(String),
    /// A value with `*`: the whole path must match it.
    Glob(String, Box<GlobMatcher>),
}

impl PathFilter {
    /// Parse a `path` argument. The error says why a value holding `*` is not a glob.
    pub fn parse(value: &str) -> Result<Self, String> {
        if !value.contains('*') {
            return Ok(Self::Prefix(value.to_string()));
        }
        let glob = GlobBuilder::new(value)
            .literal_separator(true)
            .build()
            .map_err(|error| {
                format!(
                    "`path` holds `*`, so it is read as a glob, and `{value}` is not one ({error}); e.g. `src/**/*.rs`"
                )
            })?;
        Ok(Self::Glob(value.to_string(), Box::new(glob.compile_matcher())))
    }

    /// Whether the repo-relative `path` passes the filter.
    pub fn matches(&self, path: &[u8]) -> bool {
        match self {
            Self::Prefix(prefix) => path.starts_with(prefix.as_bytes()),
            Self::Glob(_, glob) => path.to_path().is_ok_and(|path| glob.is_match(path)),
        }
    }

    /// The value as the caller passed it.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Prefix(value) | Self::Glob(value, _) => value,
        }
    }

    /// Whether the value is read as a glob.
    pub fn is_glob(&self) -> bool {
        matches!(self, Self::Glob(..))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept<'a>(filter: &str, paths: &[&'a str]) -> Vec<&'a str> {
        let filter = PathFilter::parse(filter).expect("filter parses");
        paths
            .iter()
            .copied()
            .filter(|path| filter.matches(path.as_bytes()))
            .collect()
    }

    const PATHS: &[&str] = &[
        "src/a.rs",
        "src/mcp/tools.rs",
        "src/mcp_types.rs",
        "tests/src/a.rs",
        "app/[id]/page.tsx",
        "docs/guide.md",
    ];

    #[test]
    fn a_value_without_a_star_is_a_byte_prefix_not_a_substring() {
        assert_eq!(kept("src/mcp/", PATHS), ["src/mcp/tools.rs"]);
        assert_eq!(kept("src/mc", PATHS), ["src/mcp/tools.rs", "src/mcp_types.rs"]);
        assert_eq!(
            kept("src/", PATHS),
            ["src/a.rs", "src/mcp/tools.rs", "src/mcp_types.rs"]
        );
        assert!(kept(".rs", PATHS).is_empty());
        assert!(kept("mcp/", PATHS).is_empty());
    }

    #[test]
    fn glob_characters_without_a_star_are_literal() {
        assert_eq!(kept("app/[id]/", PATHS), ["app/[id]/page.tsx"]);
        assert!(kept("src/?.rs", PATHS).is_empty());
        assert!(!PathFilter::parse("app/[id]/").expect("prefix").is_glob());
    }

    #[test]
    fn a_value_with_a_star_is_a_glob_over_the_whole_path() {
        assert_eq!(kept("src/*.rs", PATHS), ["src/a.rs", "src/mcp_types.rs"]);
        assert_eq!(
            kept("src/**/*.rs", PATHS),
            ["src/a.rs", "src/mcp/tools.rs", "src/mcp_types.rs"]
        );
        assert_eq!(kept("**/a.rs", PATHS), ["src/a.rs", "tests/src/a.rs"]);
        assert_eq!(kept("*.md", PATHS), Vec::<&str>::new());
        assert_eq!(kept("**/*.{md,tsx}", PATHS), ["app/[id]/page.tsx", "docs/guide.md"]);
        assert!(PathFilter::parse("src/*.rs").expect("glob").is_glob());
    }

    #[test]
    fn a_star_value_that_is_no_glob_is_refused_with_an_example() {
        let error = PathFilter::parse("src/[*.rs").expect_err("unclosed class");
        assert!(
            error.starts_with("`path` holds `*`, so it is read as a glob, and `src/[*.rs` is not one ("),
            "{error}"
        );
        assert!(error.ends_with("e.g. `src/**/*.rs`"), "{error}");
    }

    #[test]
    fn the_value_is_kept_as_passed() {
        assert_eq!(PathFilter::parse("src/").expect("prefix").as_str(), "src/");
        assert_eq!(PathFilter::parse("src/**").expect("glob").as_str(), "src/**");
    }
}
