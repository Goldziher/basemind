//! The engine's operations: one synchronous `impl Workspace` method per verb tool, each in its own
//! file. Their arguments and answers are the tool's own types ([`super::types`]).

/// Upper bound on a `limit` that pages a symbol list.
pub(crate) const SEARCH_LIMIT_MAX: u32 = 1000;
