//! `ServerHandler` trait implementation for [`BasemindServer`], split out of `mod.rs` to keep both
//! files under the 1000-line module cap. Behavior is unchanged: this is the same `#[tool_handler]`
//! impl the macro would generate around the hand-written `list_tools` / `call_tool` / `get_tool` /
//! prompt / completion overrides.

use rmcp::ServerHandler;
use rmcp::model::{
    CacheScope, CompleteRequestParams, CompleteResult, GetPromptRequestParams, GetPromptResponse, ListPromptsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::tool_handler;

use super::{BasemindServer, lean, tasks};

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
    /// SEP-2663 Tasks: a call that can run long ([`tasks::SLOW_CALLS`]), made by a client that
    /// declared the tasks extension, is answered inline when it finishes within
    /// [`tasks::TASK_THRESHOLD`] and with a pollable task handle only when it is still running
    /// after it. Every other case takes the normal path.
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
            let _admission = match self.heavy_admission.admit(work_class).await {
                Ok(admission) => admission,
                Err(busy) => return Ok(rmcp::model::CallToolResponse::Complete(busy.into_tool_result())),
            };
            return lean::lean_call_tool(self, &self.tool_router, request, context).await;
        }
        let work_class = if tasks::is_slow_tool(&request.name, request.arguments.as_ref()) {
            super::admission::WorkClass::Heavy
        } else {
            super::admission::WorkClass::Control
        };
        let admission = match self.heavy_admission.admit(work_class).await {
            Ok(admission) => admission,
            Err(busy) => return Ok(rmcp::model::CallToolResponse::Complete(busy.into_tool_result())),
        };
        let client_supports_tasks = context.client_capabilities().is_some_and(|caps| caps.supports_tasks());
        if client_supports_tasks && tasks::is_slow_tool(&request.name, request.arguments.as_ref()) {
            return tasks::run_slow_tool(self, request, context, admission, self.task_threshold()).await;
        }
        let should_deliver_comms = request.name != "agents";
        let cancel_aware = tasks::is_cancel_aware_read(&request.name, request.arguments.as_ref());
        let _admission = admission;
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        // Pure read-only heavy calls race the client's cancellation: the handler future (and the
        // admission permit it keeps alive alongside) is dropped the moment the request is cancelled.
        // Mutating calls never take this branch — see `tasks::CANCEL_AWARE_READS`.
        let response = if cancel_aware {
            let ct = tcc.request_context.ct.clone();
            tasks::run_until_cancelled(ct.cancelled(), self.tool_router.call(tcc)).await?
        } else {
            self.tool_router.call(tcc).await?
        };
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
    /// the request at its next await point (see [`tasks::run_slow_tool`]) and settles the task.
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

    /// `resources/list`: the static resources (see [`super::resources`]).
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
        Ok(self.list_resources_result())
    }

    /// `resources/templates/list`: the parameterised resources.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult, rmcp::ErrorData> {
        Ok(self.list_resource_templates_result())
    }

    /// `resources/read`: delegates to the same dispatchers the matching tools use.
    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, rmcp::ErrorData> {
        Ok(self.read_resource_uri(&request.uri).await?.into())
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

    fn get_info(&self) -> ServerConfig {
        server_config()
    }
}

/// What the server says about itself in answer to `initialize`. It depends on nothing a workspace
/// holds, so the relay can give the same answer while no daemon is reachable
/// (see [`super::offline`]).
pub(super) fn server_config() -> ServerConfig {
    ServerConfig::new(
        ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .enable_completions()
            .enable_tasks()
            .build(),
    )
    // Hosts cap this text (Claude Code at 4,096 chars) and truncate from the END without
    // telling the agent, so it is ordered by what must survive: what basemind is, the tools for
    // navigating code, then history, documents, memory and, last, optional coordination.
    // `instructions_stay_under_the_client_ceiling` enforces the cap.
    .with_instructions(
        "basemind is this repository's indexed context layer: a tree-sitter code map, git history at symbol \
         resolution, document search and shared memory. Its tools are always loaded; call them \
         directly, before shell `rg`/`grep`/`find`/`git` and before reading whole files. They \
         return paths, 1-based line numbers and signatures, so they cost a fraction of the tokens of \
         reading source.\n\
         Code:\n\
         - what is in a file: code_outline {\"path\":\"src/store.rs\"}, then code_read \
         {\"path\":\"src/store.rs\",\"symbol\":\"open\"} for only the body you need\n\
         - where is X defined: code_symbols {\"name\":\"open_store\"}\n\
         - what calls / implements / imports X: code_refs {\"name\":\"open_store\"}\n\
         - find a string or comment: code_grep {\"pattern\":\"cache_root(\"} (literal unless \
         \"regex\":true)\n\
         - find code by what it does: code_search {\"query\":\"retry a failed upload\"}\n\
         - find a file by name: find_files {\"query\":\"readme\"}\n\
         History: git_log {} (recent commits; add path, query, author, symbol or rev), \
         git_blame {\"path\":\"src/store.rs\",\"symbol\":\"open\"}, git_diff {} (working-tree status; \
         add path, from, to).\n\
         Documents (markdown, PDFs, config; the code tools cover code only): docs_search \
         {\"query\":\"how releases are versioned\"}.\n\
         Memory shared across sessions: memory_save {\"key\":\"decision/x\",\"value\":\"…\"}, \
         memory_search {\"query\":\"…\"}.\n\
         Index: index_status {} when a result is unexpectedly empty; index_rescan {} after edits or \
         when a tool says the index is empty.\n\
         Conventions: paths are repository-relative with forward slashes; line and end_line are \
         1-based and inclusive; lists take limit and cursor and return next_cursor; a list trimmed \
         to its token budget sets budgeted (raise max_tokens or follow next_cursor); a failure is a \
         tool result with isError naming the fix; a notice field means the result is incomplete \
         and says why. An unknown argument is rejected, never ignored.\n\
         Only when collaborating with other agents in this repository: the agents tool (mode inbox, \
         thread_list, message, post) carries scoped threads. Skip it when working alone.",
    )
}
