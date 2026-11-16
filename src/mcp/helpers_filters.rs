//! Refusals for a call that can only come back empty: a page size of zero, or filters no indexed
//! file passes.
//!
//! Answered as a clean "no results", a mistyped language or directory reads as "the code does not
//! contain it". These are errors in the call, so they are reported as such, with what would match.
//!
//! A refusal describes the index, so it is only made of a finished one: while a scan is running or
//! was left partial the tool returns its page with the lifecycle `notice` instead.

use std::collections::BTreeSet;

use memchr::memmem::Finder;
use rmcp::ErrorData as McpError;

/// Languages named in a "no such language" refusal before the rest are counted.
const LANGUAGES_LISTED_MAX: usize = 30;

/// Wildcards that suggest a `path` filter was meant as a glob. `[` and `{` are not in the list:
/// they occur in real paths (`app/[id]/page.tsx`).
const GLOB_WILDCARDS: [char; 2] = ['*', '?'];

/// Refuse `limit: 0`: it returns an empty page whose cursor points back at the same position.
pub(super) fn reject_zero_limit(tool: &str, limit: Option<u32>) -> Result<(), McpError> {
    if limit == Some(0) {
        return Err(McpError::invalid_params(
            format!("`{tool}`: `limit` must be at least 1; omit it for the default page size."),
            None,
        ));
    }
    Ok(())
}

/// How a tool matches its path filter against a repo-relative path.
pub(super) enum PathFilter<'a> {
    /// `code_grep`'s `path`: a literal substring. The finder is boxed: on x86_64 it carries its
    /// vector searcher inline (~300 bytes), against 16 for a prefix.
    Contains(&'a str, Box<Finder<'a>>),
    /// `find_files`' `path_prefix`.
    Prefix(&'a str),
}

impl<'a> PathFilter<'a> {
    pub(super) fn contains(needle: &'a str) -> Self {
        Self::Contains(needle, Box::new(Finder::new(needle.as_bytes())))
    }

    pub(super) fn matches(&self, path: &[u8]) -> bool {
        match self {
            Self::Contains(_, finder) => finder.find(path).is_some(),
            Self::Prefix(prefix) => path.starts_with(prefix.as_bytes()),
        }
    }

    fn value(&self) -> &'a str {
        match self {
            Self::Contains(value, _) | Self::Prefix(value) => value,
        }
    }

    /// `(as a predicate, as a participle)`: "no path contains", "a path containing".
    fn verbs(&self) -> (&'static str, &'static str) {
        match self {
            Self::Contains(..) => ("contains", "containing"),
            Self::Prefix(_) => ("starts with", "starting with"),
        }
    }
}

/// The filters of one call.
pub(super) struct Filters<'a> {
    pub tool: &'a str,
    pub path: Option<PathFilter<'a>>,
    pub language: Option<&'a str>,
}

/// What a set of files looked like to a call's filters.
#[derive(Default)]
pub(super) struct Tally {
    any: bool,
    path_seen: bool,
    language_seen: bool,
    /// Files passing every filter, and the first of them.
    passed: usize,
    first: Option<String>,
    languages: BTreeSet<String>,
}

impl Tally {
    /// Record one file. Returns whether it passes every filter.
    pub(super) fn see(&mut self, filters: &Filters<'_>, path: &[u8], language: &str) -> bool {
        self.any = true;
        let path_ok = filters.path.as_ref().is_none_or(|filter| filter.matches(path));
        let language_ok = filters.language.is_none_or(|wanted| wanted == language);
        self.path_seen |= path_ok;
        self.language_seen |= language_ok;
        if !self.languages.contains(language) {
            self.languages.insert(language.to_string());
        }
        if path_ok && language_ok {
            if self.first.is_none() {
                self.first = Some(String::from_utf8_lossy(path).into_owned());
            }
            self.passed += 1;
        }
        path_ok && language_ok
    }
}

impl Filters<'_> {
    pub(super) fn is_empty(&self) -> bool {
        self.path.is_none() && self.language.is_none()
    }

    /// The refusal for filters that none of the files the tool lists passes, or `None` when one
    /// does. `listed` tallies those files; `unlisted_documents` tallies the document tier for a
    /// tool that does not cover it, so the refusal can say the filters named documents.
    ///
    /// An empty index is not a filter mistake and is left to the tool's own report.
    pub(super) fn refusal(&self, listed: &Tally, unlisted_documents: Option<&Tally>) -> Option<McpError> {
        let tool = self.tool;
        if self.is_empty() || !listed.any || listed.passed > 0 {
            return None;
        }
        if let Some(documents) = unlisted_documents.filter(|documents| documents.passed > 0) {
            return Some(McpError::invalid_params(
                format!(
                    "`{tool}`: these filters match only documents ({} indexed, e.g. `{}`), and `{tool}` searches \
                     code files. Search documents with `docs_search`.",
                    documents.passed,
                    documents.first.as_deref().unwrap_or_default()
                ),
                None,
            ));
        }
        let message = match (&self.path, self.language) {
            (_, Some(wanted)) if !listed.language_seen => {
                let total = listed.languages.len();
                let mut names: Vec<&str> = listed
                    .languages
                    .iter()
                    .take(LANGUAGES_LISTED_MAX)
                    .map(String::as_str)
                    .collect();
                let more = format!("and {} more", total.saturating_sub(LANGUAGES_LISTED_MAX));
                if total > LANGUAGES_LISTED_MAX {
                    names.push(&more);
                }
                let documents = if unlisted_documents.is_some_and(|documents| documents.language_seen) {
                    format!(" `{wanted}` files here are documents: search them with `docs_search`.")
                } else {
                    String::new()
                };
                format!(
                    "`{tool}`: no indexed file has language `{wanted}`. Indexed languages: {}.{documents}",
                    names.join(", ")
                )
            }
            (Some(filter), _) if !listed.path_seen => {
                let (verb, _) = filter.verbs();
                let value = filter.value();
                if matches!(filter, PathFilter::Contains(..)) && value.contains(GLOB_WILDCARDS) {
                    format!(
                        "`{tool}`: `path` is a substring of the repo-relative path, not a glob, and no indexed \
                         path contains `{value}`. Pass a fragment such as `src/mcp/` or `.rs`; use `language` to \
                         filter by language."
                    )
                } else {
                    format!(
                        "`{tool}`: no indexed path {verb} `{value}`. The filter is literal text, not a glob; \
                         list what is indexed with `find_files`."
                    )
                }
            }
            (Some(filter), Some(wanted)) => {
                let (_, participle) = filter.verbs();
                format!(
                    "`{tool}`: no indexed `{wanted}` file has a path {participle} `{}`; each filter matches \
                     files on its own. Drop one of them.",
                    filter.value()
                )
            }
            _ => return None,
        };
        Some(McpError::invalid_params(message, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep<'a>(path: Option<&'a str>, language: Option<&'a str>) -> Filters<'a> {
        Filters {
            tool: "code_grep",
            path: path.map(PathFilter::contains),
            language,
        }
    }

    fn tally(filters: &Filters<'_>, files: &[(&str, &str)]) -> Tally {
        let mut tally = Tally::default();
        for (path, language) in files {
            tally.see(filters, path.as_bytes(), language);
        }
        tally
    }

    const CODE: &[(&str, &str)] = &[("src/a.rs", "rust"), ("tools.py", "python")];
    const DOCS: &[(&str, &str)] = &[("docs/guide.md", "markdown"), ("docs/api.md", "markdown")];

    fn message(refusal: Option<McpError>) -> String {
        refusal.expect("a refusal").message.into_owned()
    }

    #[test]
    fn filters_that_a_listed_file_passes_are_not_refused() {
        let filters = grep(Some("src/"), Some("rust"));
        assert!(filters.refusal(&tally(&filters, CODE), None).is_none());
        let unfiltered = grep(None, None);
        assert!(unfiltered.refusal(&tally(&unfiltered, CODE), None).is_none());
    }

    #[test]
    fn an_empty_index_is_not_a_filter_mistake() {
        let filters = grep(Some("src/"), None);
        assert!(
            filters
                .refusal(&Tally::default(), Some(&tally(&filters, DOCS)))
                .is_none()
        );
    }

    #[test]
    fn filters_that_match_only_documents_name_the_document_tool() {
        let by_path = grep(Some("docs/"), None);
        let text = message(by_path.refusal(&tally(&by_path, CODE), Some(&tally(&by_path, DOCS))));
        assert!(
            text.contains("match only documents (2 indexed, e.g. `docs/guide.md`)"),
            "{text}"
        );
        assert!(text.contains("`docs_search`"), "{text}");

        let by_language = grep(None, Some("markdown"));
        let text = message(by_language.refusal(&tally(&by_language, CODE), Some(&tally(&by_language, DOCS))));
        assert!(text.contains("match only documents"), "{text}");
    }

    #[test]
    fn a_document_language_under_a_path_no_document_has_still_names_the_document_tool() {
        let filters = grep(Some("src/"), Some("markdown"));
        let text = message(filters.refusal(&tally(&filters, CODE), Some(&tally(&filters, DOCS))));
        assert!(text.contains("no indexed file has language `markdown`"), "{text}");
        assert!(text.contains("Indexed languages: python, rust."), "{text}");
        assert!(text.contains("`docs_search`"), "{text}");
    }

    #[test]
    fn a_glob_shaped_path_gets_the_glob_hint_and_a_plain_one_the_listing_hint() {
        let glob = grep(Some("src/*.rs"), None);
        let text = message(glob.refusal(&tally(&glob, CODE), None));
        assert!(
            text.contains("not a glob, and no indexed path contains `src/*.rs`"),
            "{text}"
        );

        let plain = grep(Some("nope/"), None);
        let text = message(plain.refusal(&tally(&plain, CODE), None));
        assert!(text.contains("no indexed path contains `nope/`"), "{text}");
        assert!(text.contains("`find_files`"), "{text}");

        let prefix = Filters {
            tool: "find_files",
            path: Some(PathFilter::Prefix("nope/")),
            language: None,
        };
        let text = message(prefix.refusal(&tally(&prefix, CODE), None));
        assert!(text.contains("no indexed path starts with `nope/`"), "{text}");
    }

    #[test]
    fn filters_that_each_match_alone_say_so() {
        let filters = grep(Some("src/"), Some("python"));
        let text = message(filters.refusal(&tally(&filters, CODE), None));
        assert!(
            text.contains("no indexed `python` file has a path containing `src/`"),
            "{text}"
        );
    }
}
