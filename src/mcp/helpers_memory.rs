//! Dispatch for the consolidated `memory` domain tool.
//!
//! One entry point — [`run_memory`] — validates the flat [`MemoryParams`] against the selected
//! [`MemoryMode`] and delegates to the per-operation body, which stays where it already lives: the
//! core key-value store in `memory.rs`, the audit engine in `helpers_governance.rs`, the proposal
//! lifecycle in `helpers_proposals.rs`, document retrieval in `memory.rs`.
//!
//! # Why the gate is per mode, not per tool
//!
//! Unlike `web`, the `memory` tool is advertised in EVERY build: nine of its modes need
//! `--features memory` and `documents` needs `--features documents`, but an agent searching for
//! "remember this" or "search the PDFs" is better served a tool that answers "rebuild with
//! --features memory" than a tool that is not there at all. So the tool registers unconditionally
//! and the feature check happens in the body — [`not_enabled`] is the only answer a gated-off mode
//! ever returns, never an empty result an agent would read as "nothing stored".

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::mode::{MemoryMode, reject_unsupported};
use super::types_memory::MemoryParams;

/// The graceful answer for a mode whose feature this binary was not built with.
///
/// Compiled only when at least one gate can actually be off; a `--features full` build reaches
/// every mode's real body and would leave this unused.
#[cfg(not(all(feature = "memory", feature = "documents")))]
fn not_enabled(feature: &'static str) -> Result<CallToolResult, McpError> {
    Err(McpError::invalid_request(
        format!(
            "`memory` mode requires the `{feature}` feature, which is not compiled into this \
             basemind binary. Rebuild with `--features {feature}` (the published release \
             binary includes it)."
        ),
        None,
    ))
}

/// Unwrap a parameter the selected mode requires, naming the `mode`/field pair on failure.
///
/// "missing parameter" would leave the agent guessing which of two dozen siblings this mode wanted.
#[cfg(any(feature = "memory", feature = "documents"))]
fn require<T>(mode: MemoryMode, field: &str, value: Option<T>) -> Result<T, McpError> {
    value.ok_or_else(|| {
        McpError::invalid_params(
            format!("`{}`: mode=\"{mode}\" requires `{field}`", MemoryMode::DOMAIN),
            None,
        )
    })
}

/// Every optional sibling field paired with whether the caller supplied it.
///
/// One list walked against [`accepted`], rather than a hand-written reject list per mode: a field
/// added to [`MemoryParams`] but forgotten in some mode's accept list is then rejected by default,
/// which is the safe direction — the unsafe one is a parameter silently ignored.
fn supplied(p: &MemoryParams) -> [(&'static str, bool); 26] {
    [
        ("key", p.key.is_some()),
        ("value", p.value.is_some()),
        ("tags", p.tags.is_some()),
        ("embed", p.embed.is_some()),
        ("visibility", p.visibility.is_some()),
        ("prefix", p.prefix.is_some()),
        ("tag", p.tag.is_some()),
        ("limit", p.limit.is_some()),
        ("cursor", p.cursor.is_some()),
        ("query", p.query.is_some()),
        ("dry_run", p.dry_run.is_some()),
        ("include_archived", p.include_archived.is_some()),
        ("max_tokens", p.max_tokens.is_some()),
        ("format", p.format.is_some()),
        ("mime_type", p.mime_type.is_some()),
        ("scope", p.scope.is_some()),
        ("entity_category", p.entity_category.is_some()),
        ("keywords_contains", p.keywords_contains.is_some()),
        ("window", p.window.is_some()),
        ("min_support", p.min_support.is_some()),
        ("min_confidence", p.min_confidence.is_some()),
        ("max_files_per_commit", p.max_files_per_commit.is_some()),
        ("kind", p.kind.is_some()),
        ("id", p.id.is_some()),
        ("reason", p.reason.is_some()),
        ("documents config overrides", p.overrides.any()),
    ]
}

/// The sibling fields each mode accepts, beyond the required `mode`.
fn accepted(mode: MemoryMode) -> &'static [&'static str] {
    match mode {
        MemoryMode::Put => &["key", "value", "tags", "embed", "visibility"],
        MemoryMode::Get => &["key", "visibility"],
        MemoryMode::List => &["prefix", "tag", "limit", "cursor", "visibility"],
        MemoryMode::Search => &["query", "limit", "tag", "visibility"],
        MemoryMode::Delete => &["key", "visibility"],
        MemoryMode::Audit => &["key", "visibility", "dry_run", "limit", "include_archived"],
        MemoryMode::Documents => &[
            "query",
            "limit",
            "max_tokens",
            "format",
            "mime_type",
            "scope",
            "entity_category",
            "keywords_contains",
            "documents config overrides",
        ],
        MemoryMode::Mine => &["window", "min_support", "min_confidence", "max_files_per_commit"],
        MemoryMode::Proposals => &["kind", "limit", "cursor"],
        MemoryMode::Accept => &["id", "key"],
        MemoryMode::Reject => &["id", "reason"],
    }
}

/// Fail the call when it carried parameters the selected mode does not accept.
fn reject_inapplicable(p: &MemoryParams) -> Result<(), McpError> {
    let accepted = accepted(p.mode);
    let offenders: Vec<(&str, bool)> = supplied(p)
        .into_iter()
        .filter(|(field, present)| *present && !accepted.contains(field))
        .collect();
    reject_unsupported(MemoryMode::DOMAIN, p.mode.as_str(), &offenders)
}

/// Dispatch the single `memory` tool onto the per-operation helper its `mode` selects.
pub(super) async fn run_memory(state: &ServerState, params: MemoryParams) -> Result<CallToolResult, McpError> {
    reject_inapplicable(&params)?;
    match params.mode {
        MemoryMode::Documents => run_documents(state, params).await,
        _ => run_memory_ops(state, params).await,
    }
}

/// Every mode behind `--features memory`.
///
/// Takes the params by value and `take()`s each field it needs, so the `documents` arm can still
/// hand the whole object on rather than reconstructing it.
#[cfg(feature = "memory")]
async fn run_memory_ops(state: &ServerState, mut params: MemoryParams) -> Result<CallToolResult, McpError> {
    use super::types_governance::{
        MemoryAuditParams, ProposalAcceptParams, ProposalRejectParams, ProposalsListParams, ProposalsMineParams,
    };
    use super::types_memory::{
        MemoryDeleteParams, MemoryGetParams, MemoryListParams, MemoryPutParams, MemorySearchParams,
    };

    let mode = params.mode;
    let visibility = params.visibility.unwrap_or_default();

    match mode {
        MemoryMode::Documents => run_documents(state, params).await,
        MemoryMode::Put => {
            let p = MemoryPutParams {
                key: require(mode, "key", params.key.take())?,
                value: require(mode, "value", params.value.take())?,
                tags: params.tags.take(),
                embed: params.embed.unwrap_or(true),
                visibility,
            };
            super::memory::run_memory_put(state, p).await
        }
        MemoryMode::Get => {
            let p = MemoryGetParams {
                key: require(mode, "key", params.key.take())?,
                visibility,
            };
            super::memory::run_memory_get(state, p).await
        }
        MemoryMode::List => {
            let p = MemoryListParams {
                prefix: params.prefix.take(),
                tag: params.tag.take(),
                limit: params.limit,
                cursor: params.cursor.take(),
                visibility,
            };
            super::memory::run_memory_list(state, p).await
        }
        MemoryMode::Search => {
            let p = MemorySearchParams {
                query: require(mode, "query", params.query.take())?,
                limit: params.limit,
                tag: params.tag.take(),
                visibility,
            };
            super::memory::run_memory_search(state, p).await
        }
        MemoryMode::Delete => {
            let p = MemoryDeleteParams {
                key: require(mode, "key", params.key.take())?,
                visibility,
            };
            super::memory::run_memory_delete(state, p).await
        }
        MemoryMode::Audit => {
            let p = MemoryAuditParams {
                key: params.key.take(),
                visibility,
                dry_run: params.dry_run.unwrap_or(false),
                limit: params.limit,
                include_archived: params.include_archived.unwrap_or(false),
            };
            // The audit resolves symbol provenance against the in-RAM map, so it must not run
            // against a half-warmed cache — every record would read as Stale.
            state.await_cache_ready().await;
            super::helpers_governance::run_memory_audit(state, p).await
        }
        MemoryMode::Mine => {
            let p = ProposalsMineParams {
                window: params.window,
                min_support: params.min_support,
                min_confidence: params.min_confidence,
                max_files_per_commit: params.max_files_per_commit,
            };
            super::helpers_proposals::run_proposals_mine(state, p).await
        }
        MemoryMode::Proposals => {
            let p = ProposalsListParams {
                kind: params.kind.take(),
                limit: params.limit,
                cursor: params.cursor.take(),
            };
            super::helpers_proposals::run_proposals_list(state, p).await
        }
        MemoryMode::Accept => {
            let p = ProposalAcceptParams {
                id: require(mode, "id", params.id.take())?,
                key: params.key.take(),
            };
            // Accepting stamps `verified` from the live index, same warm-cache requirement.
            state.await_cache_ready().await;
            super::helpers_proposals::run_proposal_accept(state, p).await
        }
        MemoryMode::Reject => {
            let p = ProposalRejectParams {
                id: require(mode, "id", params.id.take())?,
                reason: params.reason.take(),
            };
            super::helpers_proposals::run_proposal_reject(state, p).await
        }
    }
}

#[cfg(not(feature = "memory"))]
async fn run_memory_ops(_state: &ServerState, _params: MemoryParams) -> Result<CallToolResult, McpError> {
    not_enabled("memory")
}

#[cfg(feature = "documents")]
async fn run_documents(state: &ServerState, mut params: MemoryParams) -> Result<CallToolResult, McpError> {
    let p = super::types::SearchDocumentsParams {
        query: require(MemoryMode::Documents, "query", params.query.take())?,
        limit: params.limit,
        max_tokens: params.max_tokens,
        format: params.format.take(),
        mime_type: params.mime_type.take(),
        scope: params.scope.take(),
        entity_category: params.entity_category.take(),
        keywords_contains: params.keywords_contains.take(),
        overrides: params.overrides,
    };
    super::memory::run_search_documents(state, p).await
}

#[cfg(not(feature = "documents"))]
async fn run_documents(_state: &ServerState, _params: MemoryParams) -> Result<CallToolResult, McpError> {
    not_enabled("documents")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(mode: MemoryMode) -> MemoryParams {
        MemoryParams {
            mode,
            key: None,
            value: None,
            tags: None,
            embed: None,
            visibility: None,
            prefix: None,
            tag: None,
            limit: None,
            cursor: None,
            query: None,
            dry_run: None,
            include_archived: None,
            max_tokens: None,
            format: None,
            mime_type: None,
            scope: None,
            entity_category: None,
            keywords_contains: None,
            window: None,
            min_support: None,
            min_confidence: None,
            max_files_per_commit: None,
            kind: None,
            id: None,
            reason: None,
            overrides: Default::default(),
        }
    }

    #[test]
    fn should_accept_every_mode_called_with_only_its_own_fields() {
        for mode in MemoryMode::ALL {
            let mut p = params(*mode);
            match mode {
                MemoryMode::Put => {
                    p.key = Some("k".into());
                    p.value = Some("v".into());
                }
                MemoryMode::Get | MemoryMode::Delete => p.key = Some("k".into()),
                MemoryMode::List => p.prefix = Some("skill/".into()),
                MemoryMode::Search | MemoryMode::Documents => p.query = Some("retry".into()),
                MemoryMode::Audit => p.dry_run = Some(true),
                MemoryMode::Mine => p.window = Some(50),
                MemoryMode::Proposals => p.kind = Some("skill".into()),
                MemoryMode::Accept | MemoryMode::Reject => p.id = Some("abc".into()),
            }
            assert!(reject_inapplicable(&p).is_ok(), "mode `{mode}` rejected its own field");
        }
    }

    #[test]
    fn should_name_every_field_the_mode_does_not_accept() {
        let mut p = params(MemoryMode::Get);
        p.key = Some("k".into());
        p.window = Some(10);
        p.mime_type = Some("application/pdf".into());

        let error = reject_inapplicable(&p).expect_err("fields from other modes must be rejected");
        let message = error.message.to_string();
        assert!(message.contains("`memory` mode `get` does not accept"), "{message}");
        assert!(
            message.contains("`window`") && message.contains("`mime_type`"),
            "{message}"
        );
        assert!(
            !message.contains("`key`"),
            "an accepted field must not be reported: {message}"
        );
    }

    /// `documents` shares `limit` and `query` with `search` but nothing else — a `visibility` meant
    /// for the memory tiers must not be quietly dropped on a document search.
    #[test]
    fn should_reject_a_memory_tier_on_a_document_search() {
        let mut p = params(MemoryMode::Documents);
        p.query = Some("retry".into());
        p.visibility = Some(super::super::types_memory::Visibility::Individual);
        let error = reject_inapplicable(&p).expect_err("visibility does not apply to `documents`");
        assert!(error.message.contains("`visibility`"), "{}", error.message);
    }

    #[test]
    fn should_cover_every_mode_in_the_accept_table() {
        for mode in MemoryMode::ALL {
            assert!(
                !accepted(*mode).is_empty(),
                "mode `{mode}` has no accepted fields listed"
            );
        }
    }
}
