//! `ServerHandler` trait implementation for [`BasemindServer`], split out of `mod.rs` to keep both
//! files under the 1000-line module cap. Behavior is unchanged: this is the same `#[tool_handler]`
//! impl the macro would generate around the hand-written `list_tools` / `call_tool` / `get_tool` /
//! prompt / logging / completion overrides.

use rmcp::ServerHandler;
use rmcp::model::{
    CacheScope, CompleteRequestParams, CompleteResult, GetPromptRequestParams, GetPromptResponse, ListPromptsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::tool_handler;

use super::{BasemindServer, lean, notifications, tasks};

/// SEP-2549 cache TTL advertised on `tools/list` and `prompts/list`. The advertised tool and prompt
/// sets are fixed for the lifetime of a server process (they change only with the binary/schema, not
/// with the index), so a compliant client can safely cache the schemas for this window instead of
/// re-listing every session. Scoped `Public` because the surface is not client- or user-specific.
pub(super) const LIST_CACHE_TTL_MS: u64 = 300_000;

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for BasemindServer {
    /// `tools/list`. Default (the overwhelming case): delegate to the static router exactly as
    /// the `#[tool_handler]` macro would, advertising every real tool. When `BASEMIND_MCP_LEAN`
    /// is set, advertise only the three lean wrapper tools instead. The macro detects this
    /// hand-written method and skips generating its own, so the default branch must remain a
    /// faithful copy of the generated body to keep the unset-flag surface byte-for-byte identical.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        if self.lean_enabled() {
            return Ok(lean::lean_list_tools());
        }
        Ok(
            rmcp::model::ListToolsResult::with_all_items(self.tool_router.list_all())
                .with_ttl_ms(LIST_CACHE_TTL_MS)
                .with_cache_scope(CacheScope::Public),
        )
    }

    /// `tools/call`. Default: dispatch through the static router exactly as the macro would.
    /// In lean mode, route the three wrapper tools through `lean::lean_call_tool`, which itself
    /// delegates `invoke_tool` back to this same router — no tool logic is duplicated.
    ///
    /// SEP-2663 Tasks: before the synchronous dispatch, a slow call ([`tasks::SLOW_CALLS`]) made by
    /// a client that declared the tasks extension is offloaded onto the [`TaskManager`] and answered
    /// with a pollable task handle instead of a blocked call. Every other case takes the normal path.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        if self.lean_enabled() {
            let inner_name = request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("tool_name"))
                .and_then(serde_json::Value::as_str);
            let inner_arguments = request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("tool_input"))
                .and_then(serde_json::Value::as_object);
            let work_class = if inner_name.is_some_and(|name| tasks::is_slow_tool(name, inner_arguments)) {
                super::admission::WorkClass::Heavy
            } else {
                super::admission::WorkClass::Control
            };
            let _admission = self
                .heavy_admission
                .admit(work_class)
                .await
                .map_err(rmcp::ErrorData::from)?;
            return lean::lean_call_tool(self, &self.tool_router, request, context).await;
        }
        let work_class = if tasks::is_slow_tool(&request.name, request.arguments.as_ref()) {
            super::admission::WorkClass::Heavy
        } else {
            super::admission::WorkClass::Control
        };
        let admission = self
            .heavy_admission
            .admit(work_class)
            .await
            .map_err(rmcp::ErrorData::from)?;
        let client_supports_tasks = context.client_capabilities().is_some_and(|caps| caps.supports_tasks());
        if client_supports_tasks && tasks::is_slow_tool(&request.name, request.arguments.as_ref()) {
            return Ok(rmcp::model::CallToolResponse::Task(tasks::spawn_slow_tool(
                self, request, context, admission,
            )));
        }
        let should_deliver_comms = request.name != "agents";
        let _admission = admission;
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let response = self.tool_router.call(tcc).await?;
        #[cfg(all(feature = "comms", any(unix, windows)))]
        let mut response = response;
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if should_deliver_comms
            && let rmcp::model::CallToolResponse::Complete(result) = &mut response
            && let Some(notice) = super::helpers_comms::take_delivery_notice(&self.state).await
        {
            result.content.push(rmcp::model::ContentBlock::text(notice));
        }
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        let _ = should_deliver_comms;
        Ok(response)
    }

    /// SEP-2663 `tasks/get`: report the current state of a spawned task. Thin delegation to the
    /// [`TaskManager`]; an unknown `task_id` surfaces as `-32602 Invalid params`.
    async fn get_task(
        &self,
        request: rmcp::model::GetTaskParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::GetTaskResult, rmcp::ErrorData> {
        Ok(rmcp::model::GetTaskResult::new(self.tasks.get_task(&request.task_id)?))
    }

    /// SEP-2663 `tasks/update`: deliver responses to a task's outstanding input requests. basemind's
    /// slow tools do not currently request mid-task input, so this is a compliant no-op ack for tasks
    /// with no pending inputs; the delegation keeps the door open for future `request_input` callers.
    async fn update_task(
        &self,
        request: rmcp::model::UpdateTaskParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.tasks.update_task(&request.task_id, request.input_responses)
    }

    /// SEP-2663 `tasks/cancel`: cooperative cancellation. Acks immediately; the running tool observes
    /// the request at its next await point (see [`tasks::spawn_slow_tool`]) and settles the task.
    async fn cancel_task(
        &self,
        request: rmcp::model::CancelTaskParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.tasks.cancel_task(&request.task_id)
    }

    /// `get_tool` introspection. Default mirrors the macro (router lookup); in lean mode it
    /// reports the three wrapper tools so task-support validation matches the advertised surface.
    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        if self.lean_enabled() {
            return lean::lean_get_tool(name);
        }
        self.tool_router.get(name).cloned()
    }

    /// `prompts/list`: advertise the reusable prompt templates. Delegates to the
    /// `#[prompt_router]`-built router (basemind can't use `#[prompt_handler]` — it would
    /// regenerate `get_info`).
    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        Ok(ListPromptsResult::with_all_items(self.prompt_router.list_all())
            .with_ttl_ms(LIST_CACHE_TTL_MS)
            .with_cache_scope(CacheScope::Public))
    }

    /// `prompts/get`: render one prompt template with its arguments, via the prompt router.
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<GetPromptResponse, rmcp::ErrorData> {
        let prompt_context =
            rmcp::handler::server::prompt::PromptContext::new(self, request.name, request.arguments, context);
        self.prompt_router.get_prompt(prompt_context).await
    }

    /// `logging/setLevel`: record the minimum severity the client wants. Subsequent log
    /// notifications (e.g. from `rescan`) are gated on this threshold.
    #[allow(deprecated)]
    async fn set_level(
        &self,
        request: rmcp::model::SetLevelRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.state.log_level.store(
            notifications::level_ordinal(request.level),
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    }

    /// `completion/complete`: autocomplete a prompt argument from the indexed code map (symbol
    /// names for `trace-symbol`, file paths for `explain-file`). Pure in-RAM prefix scan.
    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CompleteResult, rmcp::ErrorData> {
        self.state.await_cache_ready().await;
        Ok(self.complete_argument(&request))
    }

    #[allow(deprecated)]
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_completions()
                .enable_logging()
                .enable_tasks()
                .build(),
        )
        // Budgeted to stay under the 2048-char ceiling clients truncate at. The previous text ran
        // ~6.5k, so roughly two thirds was silently discarded — and because truncation takes the
        // HEAD, everything from the agent-comms contract onward never reached any agent at all.
        // Per-tool routing detail belongs in the tool descriptions (which is what deferred-tool
        // search actually matches on); this text carries only what a client must read up front:
        // what basemind is, the prefer-basemind-over-grep directive, and the coordination contract.
        // Keep any edit under the ceiling — `instructions_stay_under_the_client_ceiling` enforces it.
        .with_instructions(
            "basemind is this repository's indexed context layer: a tree-sitter code map (symbols, \
             references, callers, call graphs), git history and blame at symbol resolution, \
             full-text and semantic search, document RAG, and shared cross-session memory. The \
             index lives in a machine-global cache keyed by workspace (override \
             BASEMIND_DATA_HOME); nothing is written into the repo, and a daemon is its sole \
             writer, so any number of sessions read and write concurrently.\n\
             Every tool takes a required mode; read its description for the modes it offers.\n\
             basemind first, shell/grep/git fallback. These tools return paths, line numbers, and \
             signatures rather than file bodies, so they cost a fraction of the tokens of reading \
             source. Prefer code mode outline over opening a file, then read only the span you \
             need; code mode symbols over grep for a definition; code modes references and callers \
             over grepping call sites; code mode grep over ripgrep; code mode find to locate a file \
             by name; git modes recent, blame, blame_symbol, diff and touching over git log or git \
             blame; graph modes calls, neighbors and map to see what reaches what; memory mode \
             documents over opening PDFs; web modes scrape, crawl and map for the web. Do not \
             re-read a file basemind already mapped. Run admin mode rescan after edits; if a tool \
             reports no indexed files, run basemind scan first.\n\
             You may be one of several agents in this repo, so coordinate rather than assuming you \
             are alone. Coordination runs over threads: scoped conversations addressed by at least \
             two of subject, path-glob, and members, discovered by scope and never globally, and \
             joined explicitly. On start call agents mode inbox, then mode thread_list; both return \
             front-matter only, so use mode message with an id to read a body. Use mode post when \
             you begin, finish, or hit a decision, reply with reply_to to messages about your work, \
             and poll again while you work so replies do not go stale.\n\
             All paths are repository-relative with forward-slash separators. Paginate by passing a \
             response's next_cursor back as cursor.",
        )
    }
}
