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
    /// Symbol name (function / type / method) to trace. Matched as a substring, like
    /// `search_symbols`.
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
             1. Call `repo_info` and `status` for the language mix, file count, and index state.\n\
             2. Call `hot_files` to find the most-churned files — the code that matters most.\n\
             3. Call `recent_changes` to see what's been happening lately.\n\
             4. For each hot file, `outline` it (don't read it whole) and summarize its role.\n\
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
                 1. `search_symbols` for `{symbol}` to find its definition(s) — note path + \
                 signature.\n\
                 2. `find_references` for `{symbol}` to see every call site.\n\
                 3. `find_callers` on the definition to get the scope-resolved callers.\n\
                 4. `blame_symbol` to see who last changed it and when.\n\
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
                 1. `outline` `{path}` (add `l2: true`) for its symbols, signatures, imports, and \
                 calls — do NOT read the whole file yet.\n\
                 2. From the outline, identify the few symbols that carry the file's purpose.\n\
                 3. Only then read the specific line spans you need to explain those symbols.\n\
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
             1. `working_tree_status` for the staged / unstaged / untracked breakdown.\n\
             2. For each changed file, `diff_outline` to see which symbols changed structurally \
             (not just line noise).\n\
             3. For the non-trivial changes, `blame_symbol` / `commits_touching` to recover the \
             prior intent of the code being modified.\n\
             Give me a focused review: what changed and why it matters, risks or regressions, and \
             anything that looks unfinished. Cite paths and symbols.",
        )]
    }
}
