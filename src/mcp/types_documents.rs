//! The response of `docs_search`: a page of the document search engine
//! ([`crate::lance::docs_page`]) plus the MCP paging and timing fields.

#[cfg(feature = "documents")]
use serde::Serialize;

#[cfg(feature = "documents")]
#[derive(Debug, Serialize)]
pub(crate) struct DocsSearchResponse {
    #[serde(flatten)]
    pub page: crate::lance::docs_types::DocsPage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<super::cursor::Cursor>,
    pub elapsed_us: u64,
}
