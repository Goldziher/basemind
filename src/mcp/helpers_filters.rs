//! Refusals for a call that can only come back empty: a page size of zero, or filters no indexed
//! file passes.
//!
//! Answered as a clean "no results", a mistyped language or directory reads as "the code does not
//! contain it". These are errors in the call, so they are reported as such, with what would match.

use std::collections::BTreeSet;

use rmcp::ErrorData as McpError;

use super::MapCache;

/// Languages named in a "no such language" refusal before the rest are counted.
const LANGUAGES_LISTED_MAX: usize = 30;

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
#[derive(Clone, Copy)]
pub(super) enum PathFilter<'a> {
    /// `code_grep`'s `path`: a literal substring.
    Contains(&'a str),
    /// `find_files`' `path_prefix`.
    Prefix(&'a str),
}

impl PathFilter<'_> {
    fn matches(self, path: &[u8]) -> bool {
        match self {
            Self::Contains(needle) => memchr::memmem::find(path, needle.as_bytes()).is_some(),
            Self::Prefix(prefix) => path.starts_with(prefix.as_bytes()),
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::Contains(_) => "contains",
            Self::Prefix(_) => "starts with",
        }
    }

    fn value(self) -> String {
        match self {
            Self::Contains(value) | Self::Prefix(value) => value.to_string(),
        }
    }
}

/// The refusal for filters that no indexed file passes, or `None` when one does.
///
/// An empty index is not a filter mistake and is left to the tool's own report. `documents` adds
/// the document tier, for the tools that list it.
pub(super) fn selects_no_file(
    tool: &str,
    cache: &MapCache,
    documents: bool,
    path: Option<PathFilter<'_>>,
    language: Option<&str>,
) -> Option<McpError> {
    if path.is_none() && language.is_none() {
        return None;
    }
    let mut languages: BTreeSet<String> = BTreeSet::new();
    let (mut path_seen, mut language_seen, mut any) = (false, false, false);
    let mut passes = |file: &[u8], file_language: &str| {
        any = true;
        let path_ok = path.is_none_or(|filter| filter.matches(file));
        let language_ok = language.is_none_or(|wanted| wanted == file_language);
        path_seen |= path_ok;
        language_seen |= language_ok;
        if !languages.contains(file_language) {
            languages.insert(file_language.to_string());
        }
        path_ok && language_ok
    };
    for (file, meta) in cache.file_metas() {
        if passes(file.as_bytes(), &meta.language) {
            return None;
        }
    }
    if documents {
        for file in cache.doc_paths.iter() {
            let file_language = file
                .as_str()
                .map_or_else(|| "document".to_string(), super::helpers_files::doc_language);
            if passes(file.as_bytes(), &file_language) {
                return None;
            }
        }
    }
    if !any {
        return None;
    }

    let message = match (path, language) {
        (_, Some(wanted)) if !language_seen => {
            let total = languages.len();
            let mut listed: Vec<String> = languages.into_iter().take(LANGUAGES_LISTED_MAX).collect();
            if total > LANGUAGES_LISTED_MAX {
                listed.push(format!("and {} more", total - LANGUAGES_LISTED_MAX));
            }
            format!(
                "`{tool}`: no indexed file has language `{wanted}`. Indexed languages: {}.",
                listed.join(", ")
            )
        }
        (Some(filter), _) if !path_seen => format!(
            "`{tool}`: no indexed path {} `{}`. The filter is literal text, not a glob; list what is indexed \
             with `find_files`.",
            filter.text(),
            filter.value()
        ),
        (Some(filter), Some(wanted)) => format!(
            "`{tool}`: no indexed `{wanted}` file has a path {} `{}`; each filter matches files on its own. Drop \
             one of them.",
            match filter {
                PathFilter::Contains(_) => "containing",
                PathFilter::Prefix(_) => "starting with",
            },
            filter.value()
        ),
        _ => return None,
    };
    Some(McpError::invalid_params(message, None))
}
