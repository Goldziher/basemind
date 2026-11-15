//! Reusable MCP prompt templates (`prompts/list` + `prompts/get`).
//!
//! Prompts are short, parameterized workflows that teach a client how to drive basemind's tools
//! for a common task — "get oriented in this repo", "trace a symbol", "review my uncommitted
//! changes". They complement the big `instructions` string in [`super`]`::get_info` with concrete,
//! selectable starting points, and their typed arguments (`symbol`, `path`) are the references the
//! `completion/complete` handler autocompletes (see [`super::completions`]).
//!
//! The router is built by the `#[prompt_router]` macro into `Self::prompt_router()` and stored on
//! [`super::BasemindServer`]; `list_prompts` / `get_prompt` in `super` delegate to it manually
//! (basemind hand-writes its `ServerHandler` impl for lean-mode, so it cannot use the blanket
//! `#[prompt_handler]` macro, which would regenerate `get_info`).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{PromptMessage, Role};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{prompt, prompt_router};
use serde::{Deserialize, Serialize};

use super::BasemindServer;

/// Argument for the `trace-symbol` prompt: the symbol name to follow through references, callers,
/// and blame. Autocompleted by the completions handler from the `symbols_by_name` index.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TraceSymbolArgs {
    /// Symbol name (function / type / method) to trace, as `code_symbols` takes it.
    pub symbol: String,
}

/// Argument for the `explain-file` prompt: the repo-relative path to outline and explain.
/// Autocompleted by the completions handler from the indexed file list.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExplainFileArgs {
    /// Repo-relative path of the file to outline first, then read selectively.
    pub path: String,
}

#[prompt_router(vis = "pub(super)", router = "prompt_router")]
impl BasemindServer {
    /// Orientation workflow for a repository the agent has not seen before. No arguments.
    #[prompt(
        name = "onboard-repo",
        description = "Get oriented in this repository using basemind: language mix, hot files, \
                       and recent activity — structure first, source second."
    )]
    pub async fn onboard_repo_prompt(&self) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            "Help me get oriented in this repository. Work structure-first, using basemind \
             tools rather than reading files:\n\
             1. `index_status` for the language mix, file count, branch and index state.\n\
             2. `git_log` with no arguments to see what has been happening lately and which \
             files keep changing.\n\
             3. `find_files` with a `path_prefix` to see how the tree is laid out.\n\
             4. For each file that keeps changing, `code_outline` (don't read it whole) and \
             summarize its role.\n\
             Then give me a short map of the codebase: its main components, where the activity is, \
             and where I'd start to make a change. Cite file paths.",
        )]
    }

    /// Trace one symbol through the code map: definition, references, callers, and blame.
    #[prompt(
        name = "trace-symbol",
        description = "Trace a symbol through the code map — definition, references, callers, and \
                       who last changed it — without reading whole files."
    )]
    pub async fn trace_symbol_prompt(&self, Parameters(args): Parameters<TraceSymbolArgs>) -> Vec<PromptMessage> {
        let symbol = args.symbol;
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Trace the symbol `{symbol}` through this codebase using basemind:\n\
                 1. `code_symbols` with name `{symbol}` to find its definition(s): note path, \
                 line and signature.\n\
                 2. `code_refs` with name `{symbol}` to see every call site.\n\
                 3. `code_refs` again with the definition's `path` to mark the callers that \
                 resolve to that definition.\n\
                 4. `git_blame` with that `path` and `symbol` to see who last changed it and when.\n\
                 Summarize what `{symbol}` is, who depends on it, and the blast radius of changing \
                 it. Cite paths and line numbers; do not read whole files."
            ),
        )]
    }

    /// Explain one file by outlining it first, then reading only the spans that matter.
    #[prompt(
        name = "explain-file",
        description = "Explain a file structure-first: outline its symbols and imports, then read \
                       only the spans that matter."
    )]
    pub async fn explain_file_prompt(&self, Parameters(args): Parameters<ExplainFileArgs>) -> Vec<PromptMessage> {
        let path = args.path;
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Explain the file `{path}` using basemind, structure-first:\n\
                 1. `code_outline` on `{path}` for its symbols, signatures, line spans and \
                 imports. Do NOT read the whole file yet.\n\
                 2. From the outline, identify the few symbols that carry the file's purpose.\n\
                 3. Only then `code_read` those symbols (or their line spans) to explain them.\n\
                 Give me a concise explanation of what `{path}` does and how it fits the codebase, \
                 citing symbol names and line numbers."
            ),
        )]
    }

    /// Review the uncommitted working-tree changes. No arguments.
    #[prompt(
        name = "review-working-tree",
        description = "Review the uncommitted changes in the working tree: what changed, the \
                       structural diff, and prior history of the touched code."
    )]
    pub async fn review_working_tree_prompt(&self) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            "Review my uncommitted changes using basemind:\n\
             1. `git_diff` with no arguments for the staged / unstaged / untracked breakdown.\n\
             2. For each changed file, `git_diff` with its `path` and `structural: true` to see \
             which symbols were added or removed (not just line noise).\n\
             3. For the non-trivial changes, `git_blame` with a `symbol` and `git_log` with the \
             `path` to recover the prior intent of the code being modified.\n\
             Give me a focused review: what changed and why it matters, risks or regressions, and \
             anything that looks unfinished. Cite paths and symbols.",
        )]
    }
}
