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
    /// SEP-2663 Tasks: before the synchronous dispatch, a slow tool ([`tasks::SLOW_TOOLS`]) called by
    /// a client that declared the tasks extension is offloaded onto the [`TaskManager`] and answered
    /// with a pollable task handle instead of a blocked call. Every other case takes the normal path.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        if self.lean_enabled() {
            return lean::lean_call_tool(self, &self.tool_router, request, context).await;
        }
        let client_supports_tasks = context.client_capabilities().is_some_and(|caps| caps.supports_tasks());
        if client_supports_tasks && tasks::is_slow_tool(&request.name) {
            return Ok(rmcp::model::CallToolResponse::Task(tasks::spawn_slow_tool(
                self, request, context,
            )));
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
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
        .with_instructions(
            "basemind is the indexed context layer for this repository, served over MCP: a \
             tree-sitter code map across 300+ languages (symbols, references, callers, call \
             graphs, implementations), git history + blame at symbol resolution, full-text + \
             semantic search, document RAG over 90+ file formats, and shared cross-session \
             memory. The index lives in a machine-global cache (the platform data directory — \
             `~/Library/Application Support/basemind/` on macOS, `~/.local/share/basemind/` on \
             Linux; override with `BASEMIND_DATA_HOME`) keyed by workspace — nothing is written \
             into the repo — and a background daemon is the sole writer, so any number of sessions on \
             this repo read and write concurrently. basemind first, shell/grep/git fallback: \
             prefer these tools over reading files, over grep, and over naked `git` — and use \
             them for document extraction, web crawling, and code parsing too. You may be one of \
             several agents in this repo: on start, check your inbox and the threads scoped to \
             where you're working, and post status as you go (see Agent comms below).\n\
             Context economy — these tools return paths, line numbers, and signatures, not \
             file bodies, so they cost a fraction of the tokens of reading source. Default to \
             them: `outline` a file before you open it (then read only the span you need); \
             `search_symbols` instead of grep for a definition; `find_references` / \
             `find_callers` instead of grepping call sites; `workspace_grep` instead of \
             shelling out to ripgrep; `rescan` after edits instead of reconnecting. Do not \
             re-read a file basemind already mapped. Same discipline beyond code: use the git \
             tools (`recent_changes` / `blame_*` / `diff_*` / `commits_touching`) instead of \
             shelling out to `git log`/`git blame`; `search_documents` and the documents \
             pipeline for extraction, RAG, keyword + entity (NER), and summary instead of \
             opening files; `web_scrape` / `web_crawl` / `web_map` for scraping, crawling, and \
             sitemaps.\n\
             Routing: \
             \"where is X defined?\" → `search_symbols`; \
             \"what calls X?\" → `find_references` (any name) or `find_callers` (specific def); \
             \"shape of this file?\" → `outline` (add `l2: true` for calls + docs); \
             \"what changed recently?\" → `recent_changes`, `commits_touching`, `symbol_history`; \
             \"who last touched this?\" → `blame_file` / `blame_symbol`; \
             \"where's the churn?\" → `hot_files`; \
             \"semantic search across PDFs/docs in the repo?\" → `search_documents`; \
             \"recall something the agent remembered earlier?\" → `memory_get` / `memory_list` / \
             `memory_search`; \
             \"remember this for later sessions?\" → `memory_put` (delete with `memory_delete`); \
             \"refresh the index after editing code?\" → `rescan` (or `rescan { paths: [...] }` \
             to limit to changed files); \
             \"any other agents working here / leave a note for the next session?\" → \
             `inbox_read` / `thread_list` / `thread_post`.\n\
             \"find a file by name/path when I only remember a fragment?\" → `find_files` \
             (fuzzy, fzf/fd-style).\n\
             \"which repos/worktrees/branches does the daemon know?\" → `workspaces` / \
             `worktrees` / `branches`; claim a worktree so sessions don't collide with \
             `worktree_claim` (release with `worktree_release`).\n\
             \"got a truncated result? fetch the next page?\" → pass `next_cursor` from the prior \
             response back as `cursor`.\n\
             \"need regex over file contents?\" → `workspace_grep`.\n\
             Code-map tools: `outline`, `search_symbols`, `find_references`, `find_callers`, \
             `list_files`, `find_files`, `workspace_grep`, `dependents`, `status`, `repo_info`, \
             `symbol_history`. \
             Coordination tools: `workspaces`, `worktrees`, `branches`, `worktree_claim`, \
             `worktree_release` (advisory claims across the daemon's known worktrees). \
             Git tools (inside a repo): `working_tree_status`, `recent_changes`, `commits_touching`, \
             `find_commits_by_path`, `hot_files`, `diff_outline`, `diff_file`, `blame_file`, \
             `blame_symbol`. \
             Intelligence tools (require build with `--features documents,memory`): \
             `search_documents`, `memory_put`, `memory_get`, `memory_list`, `memory_search`, \
             `memory_delete`. \
             Web tools (require build with `--features crawl`): `web_scrape` (one URL), \
             `web_crawl` (follow links from a seed URL), `web_map` (sitemap-only discovery). \
             Crawled pages land in the same LanceDB documents table as on-disk docs, scoped \
             under `web:<host>` — find them later with `search_documents`. \
             Agent comms (require build with `--features comms`): coordinate with other agents \
             via THREADS — scoped conversations addressed by at least two of {subject, \
             path-glob, members}. Threads are discovered by scope (you're a member, your cwd \
             matches the thread's path-glob, or a subject filter) — never globally — and you \
             must explicitly `thread_join` to participate; there is no auto-join. On start, \
             `inbox_read` (front-matter only: subject / from / id — call `message_get` with an \
             id for a body) and `thread_list` to see threads in scope; skim `thread_history` on \
             the relevant one. `thread_start {subject, path_glob?, members?}` opens a thread \
             (you're the creator/admin; a human is also admin); `thread_post {thread, subject, \
             body, reply_to?}` when you begin, finish, or hit a decision, and reply \
             (`reply_to`) to messages about your work — do not stay silent when collaborating. \
             `inbox_ack` clears read messages. Idle threads auto-archive; `thread_archive` \
             closes one explicitly. Tools: `thread_start`, `thread_list`, `thread_join`, \
             `thread_leave`, `thread_members`, `thread_add_member`, `thread_remove_member`, \
             `thread_archive`, `thread_post`, `thread_history`, `message_get`, `inbox_read`, \
             `inbox_ack`, `agent_register`, `agent_list`. \
             All paths are repository-relative with forward-slash separators. \
             If a tool reports \"no indexed files\", run `basemind scan` in the repo first.",
        )
    }
}
