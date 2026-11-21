//! Refusals for a call that can only come back empty: a page size of zero, or filters no indexed
//! file passes.
//!
//! Answered as a clean "no results", a mistyped language or directory reads as "the code does not
//! contain it". These are errors in the call, so they are reported as such, with what would match.
//!
//! A refusal describes the index, so it is only made of a finished one: while a scan is running or
//! was left partial the tool returns its page with the lifecycle `notice` instead.

use std::collections::BTreeSet;

use crate::engine::MapCache;
use crate::engine::error::EngineError;
use crate::engine::types::LifecycleNotice;
use crate::engine::workspace::Workspace;
use crate::path_filter::PathFilter;

/// Languages named in a "no such language" refusal before the rest are counted.
const LANGUAGES_LISTED_MAX: usize = 30;

/// Parse `tool`'s `path` argument ([`PathFilter`]); a value holding `*` that is no glob is an
/// invalid argument.
pub(crate) fn parse_path(tool: &str, value: Option<&str>) -> Result<Option<PathFilter>, EngineError> {
    value
        .map(|value| PathFilter::parse(value).map_err(|why| EngineError::invalid_argument(format!("`{tool}`: {why}."))))
        .transpose()
}

/// The filters of one call.
pub(crate) struct Filters<'a> {
    pub tool: &'a str,
    pub path: Option<&'a PathFilter>,
    pub language: Option<&'a str>,
}

/// What a set of files looked like to a call's filters.
#[derive(Default)]
pub(crate) struct Tally {
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
    pub(crate) fn see(&mut self, filters: &Filters<'_>, path: &[u8], language: &str) -> bool {
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
    pub(crate) fn is_empty(&self) -> bool {
        self.path.is_none() && self.language.is_none()
    }

    /// Whether a file at `path` in `language` passes every filter.
    pub(crate) fn passes(&self, path: &[u8], language: &str) -> bool {
        self.path.is_none_or(|filter| filter.matches(path)) && self.language.is_none_or(|wanted| wanted == language)
    }

    /// The refusal for filters that none of the files the tool lists passes, or `None` when one
    /// does. `listed` tallies those files; `unlisted_documents` tallies the document tier for a
    /// tool that does not cover it, so the refusal can say the filters named documents.
    ///
    /// An empty index is not a filter mistake and is left to the tool's own report.
    pub(crate) fn refusal(&self, listed: &Tally, unlisted_documents: Option<&Tally>) -> Option<EngineError> {
        let tool = self.tool;
        if self.is_empty() || !listed.any || listed.passed > 0 {
            return None;
        }
        if let Some(documents) = unlisted_documents.filter(|documents| documents.passed > 0) {
            return Some(EngineError::invalid_argument(format!(
                "`{tool}`: these filters match only documents ({} indexed, e.g. `{}`), and `{tool}` searches \
                     code and text files, not these. Search documents with `docs_search`.",
                documents.passed,
                documents.first.as_deref().unwrap_or_default()
            )));
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
                let value = filter.as_str();
                if filter.is_glob() {
                    format!(
                        "`{tool}`: no indexed path matches the glob `{value}`; `*` stays within one segment and \
                         `**` crosses `/`. List what is indexed with `find_files`."
                    )
                } else {
                    format!(
                        "`{tool}`: no indexed path starts with `{value}`; `path` is a repo-relative prefix, or a \
                         glob when it holds `*`. List what is indexed with `find_files`."
                    )
                }
            }
            (Some(filter), Some(wanted)) => {
                let participle = if filter.is_glob() { "matching" } else { "starting with" };
                format!(
                    "`{tool}`: no indexed `{wanted}` file has a path {participle} `{}`; each filter matches \
                     files on its own. Drop one of them.",
                    filter.as_str()
                )
            }
            _ => return None,
        };
        Some(EngineError::invalid_argument(message))
    }
}

/// [`doc_language`] of an indexed document path; `document` for one that is not UTF-8.
pub(crate) fn language_of_doc(path: &crate::path::RelPath) -> String {
    path.as_str().map_or_else(|| "document".to_string(), doc_language)
}

/// Whether `notice` says files may be missing from the listing, so that "no file passes the
/// filter" proves nothing yet: an index that is warming, building, rebuilding or empty. Those are
/// the notices that ask for a retry. A rescan in progress and capped reference projections leave
/// the file listing whole, and the second lasts for the session.
fn listing_incomplete(notice: &LifecycleNotice) -> bool {
    notice.retry
}

/// The refusal for filters that selected none of the files a tool lists, or `None` when there is
/// nothing to refuse: no filter, an empty index, or an index still being built, whose page carries
/// the lifecycle `notice` instead. `listed` tallies the files the tool lists, from the source it
/// lists them from; it runs only on this path, after the listing itself came back empty.
pub(crate) fn selects_no_file(
    ws: &Workspace,
    filters: &Filters<'_>,
    listed: impl FnOnce() -> Tally,
    unlisted_documents: Option<&dyn Fn() -> Tally>,
) -> Option<EngineError> {
    if filters.is_empty() || ws.lifecycle_notice().as_ref().is_some_and(listing_incomplete) {
        return None;
    }
    let documents = unlisted_documents.map(|tally| tally());
    filters.refusal(&listed(), documents.as_ref())
}

/// [`selects_no_file`] for `code_grep`, which searches the code map and not the document tier.
pub(crate) fn grep_selects_no_file(ws: &Workspace, cache: &MapCache, filters: &Filters<'_>) -> Option<EngineError> {
    let listed = || {
        let mut tally = Tally::default();
        for (path, meta) in cache.file_metas() {
            tally.see(filters, path.as_bytes(), &meta.language);
        }
        tally
    };
    let documents = || {
        let mut tally = Tally::default();
        for path in cache.doc_paths.iter() {
            tally.see(filters, path.as_bytes(), &language_of_doc(path));
        }
        tally
    };
    selects_no_file(ws, filters, listed, Some(&documents))
}

/// Language label for a document-tier path: the grammar name when one resolves (`markdown`,
/// `json`, `yaml` ...), else the lowercased extension (`pdf`, `docx`), else `document`.
pub(crate) fn doc_language(path: &str) -> String {
    if let Some(l) = crate::lang::detect(std::path::Path::new(path)) {
        return l.to_string();
    }
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(|| "document".to_string(), str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Lifecycle;

    #[test]
    fn a_filter_refusal_is_held_back_only_while_files_may_be_missing() {
        let of = |state| LifecycleNotice::for_state(state).expect("notice");
        for incomplete in [
            of(Lifecycle::WarmingUp),
            of(Lifecycle::BuildingIndex),
            LifecycleNotice::index_rebuilding(),
            LifecycleNotice::empty_index(),
        ] {
            assert!(listing_incomplete(&incomplete), "{}", incomplete.state);
        }
        let rescanning = of(Lifecycle::Rescanning);
        assert!(!listing_incomplete(&rescanning), "{}", rescanning.state);
    }

    #[test]
    fn doc_language_labels() {
        assert_eq!(doc_language("docs/README.md"), "markdown");
        assert_eq!(doc_language("a/b.pdf"), "pdf");
        assert_eq!(doc_language("LICENSEFILE"), "document");
    }

    fn filter(value: &str) -> PathFilter {
        PathFilter::parse(value).expect("filter parses")
    }

    fn grep<'a>(path: Option<&'a PathFilter>, language: Option<&'a str>) -> Filters<'a> {
        Filters {
            tool: "code_grep",
            path,
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

    fn message(refusal: Option<EngineError>) -> String {
        refusal.expect("a refusal").to_string()
    }

    #[test]
    fn filters_that_a_listed_file_passes_are_not_refused() {
        let src = filter("src/");
        let filters = grep(Some(&src), Some("rust"));
        assert!(filters.refusal(&tally(&filters, CODE), None).is_none());
        let unfiltered = grep(None, None);
        assert!(unfiltered.refusal(&tally(&unfiltered, CODE), None).is_none());
    }

    #[test]
    fn an_empty_index_is_not_a_filter_mistake() {
        let src = filter("src/");
        let filters = grep(Some(&src), None);
        assert!(
            filters
                .refusal(&Tally::default(), Some(&tally(&filters, DOCS)))
                .is_none()
        );
    }

    #[test]
    fn filters_that_match_only_documents_name_the_document_tool() {
        let docs = filter("docs/");
        let by_path = grep(Some(&docs), None);
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
        let src = filter("src/");
        let filters = grep(Some(&src), Some("markdown"));
        let text = message(filters.refusal(&tally(&filters, CODE), Some(&tally(&filters, DOCS))));
        assert!(text.contains("no indexed file has language `markdown`"), "{text}");
        assert!(text.contains("Indexed languages: python, rust."), "{text}");
        assert!(text.contains("`docs_search`"), "{text}");
    }

    #[test]
    fn a_path_no_file_has_is_refused_with_the_rule_it_was_read_by() {
        let glob = filter("src/*.py");
        let filters = grep(Some(&glob), None);
        assert_eq!(
            message(filters.refusal(&tally(&filters, CODE), None)),
            "`code_grep`: no indexed path matches the glob `src/*.py`; `*` stays within one \
             segment and `**` crosses `/`. List what is indexed with `find_files`."
        );

        // A substring of an indexed path is not its prefix.
        let fragment = filter("a.rs");
        let filters = grep(Some(&fragment), None);
        assert_eq!(
            message(filters.refusal(&tally(&filters, CODE), None)),
            "`code_grep`: no indexed path starts with `a.rs`; `path` is a repo-relative \
             prefix, or a glob when it holds `*`. List what is indexed with `find_files`."
        );
    }

    #[test]
    fn filters_that_each_match_alone_say_so() {
        let src = filter("src/");
        let filters = grep(Some(&src), Some("python"));
        let text = message(filters.refusal(&tally(&filters, CODE), None));
        assert!(
            text.contains("no indexed `python` file has a path starting with `src/`"),
            "{text}"
        );

        let glob = filter("**/*.rs");
        let filters = grep(Some(&glob), Some("python"));
        let text = message(filters.refusal(&tally(&filters, CODE), None));
        assert!(
            text.contains("no indexed `python` file has a path matching `**/*.rs`"),
            "{text}"
        );
    }

    #[test]
    fn a_star_value_that_is_no_glob_is_an_invalid_argument_naming_the_tool() {
        let error = parse_path("find_files", Some("src/[*.rs")).expect_err("no glob");
        assert!(
            error
                .to_string()
                .starts_with("`find_files`: `path` holds `*`, so it is read as a glob"),
            "{error}"
        );
        assert!(parse_path("find_files", None).expect("no filter").is_none());
    }
}
