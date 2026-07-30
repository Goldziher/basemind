//! Process-wide memoization of [`BasemindServer`]'s tool + prompt routers.
//!
//! [`BasemindServer::assemble_router`] and [`BasemindServer::prompt_router`] take no arguments and
//! depend only on compile-time feature flags — their output is identical for every server
//! instance in this process, no matter which [`SharedReadStack`](super::SharedReadStack) it wraps.
//! Every constructor used to call them fresh, which rebuilds the ~79-tool router from scratch:
//! one `Arc::new` dispatch-closure allocation and one name validation per tool, then two `HashMap`
//! inserts (primary + disabled-set) per tool as the per-area routers are `+`-combined. In
//! stateless streamable-HTTP mode, [`BasemindServer::from_shared`] runs once per POST, so that
//! rebuild was previously a per-request cost.
//!
//! Both `ToolRouter<S>` and `PromptRouter<S>` are `Clone` (an `Arc`-refcount bump per route plus a
//! `HashMap`/`HashSet` copy of the small, cheaply-cloneable route entries) and `Send + Sync`
//! (routes dispatch on the `&BasemindServer` passed in per-call via `ToolCallContext`/
//! `PromptContext`, not on state captured at build time). That makes a `OnceLock`-memoized
//! singleton, cloned per construction, safe and behavior-preserving: byte-identical tool/prompt
//! sets, just built once instead of once per server.
use std::sync::OnceLock;

use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;

use super::BasemindServer;

static TOOL_ROUTER: OnceLock<ToolRouter<BasemindServer>> = OnceLock::new();
static PROMPT_ROUTER: OnceLock<PromptRouter<BasemindServer>> = OnceLock::new();

/// The shared, memoized full tool router. Built once per process (first caller pays the assembly
/// cost); every subsequent call is a cheap clone of the cached instance.
pub(super) fn cached_tool_router() -> ToolRouter<BasemindServer> {
    TOOL_ROUTER.get_or_init(BasemindServer::assemble_router).clone()
}

/// The shared, memoized prompt router. Same memoization shape as [`cached_tool_router`].
pub(super) fn cached_prompt_router() -> PromptRouter<BasemindServer> {
    PROMPT_ROUTER.get_or_init(BasemindServer::prompt_router).clone()
}
