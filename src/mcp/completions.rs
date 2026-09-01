//! Argument completion (`completion/complete`) for prompt arguments.
//!
//! MCP completion is scoped to a *reference* — a prompt or a resource template — not to arbitrary
//! tool arguments. basemind backs the typed arguments of its [`super::prompts`] templates from the
//! in-RAM code map: the `trace-symbol` prompt's `symbol` argument completes against indexed symbol
//! names, and the `explain-file` prompt's `path` argument completes against indexed file paths.
//! Both sources are the `MapCache` snapshot, so completion takes no store lock. Paths answer from
//! the resident file view; symbol names stream the outlines, which is RAM-only whenever they fit
//! the `[resources] max_map_cache_mb` budget and otherwise re-reads their blobs.

use std::collections::BTreeSet;

use rmcp::model::{CompleteRequestParams, CompleteResult, CompletionInfo, Reference};

use super::BasemindServer;

/// MCP caps a completion response at 100 values; we return at most this many.
const MAX_COMPLETIONS: usize = 100;

impl BasemindServer {
    /// Resolve a `completion/complete` request into up to [`MAX_COMPLETIONS`] candidate values.
    /// Only prompt-argument references are completed; resource references (basemind exposes no
    /// resources) yield an empty list.
    pub(super) fn complete_argument(&self, params: &CompleteRequestParams) -> CompleteResult {
        let values = match &params.r#ref {
            Reference::Prompt(prompt) => {
                self.complete_prompt_argument(&prompt.name, &params.argument.name, &params.argument.value)
            }
            Reference::Resource(_) => Vec::new(),
            // `Reference` is #[non_exhaustive] in rmcp 2.1; basemind exposes no resources and
            _ => Vec::new(),
        };
        let info = CompletionInfo::new(values).unwrap_or_default();
        CompleteResult::new(info)
    }

    /// Dispatch on `(prompt, argument)` to the matching code-map source. Unknown pairs (a prompt
    /// with no completable argument) return nothing rather than guessing.
    fn complete_prompt_argument(&self, prompt: &str, argument: &str, value: &str) -> Vec<String> {
        match (prompt, argument) {
            ("trace-symbol", "symbol") => self.complete_symbol_names(value),
            ("explain-file", "path") => self.complete_file_paths(value),
            _ => Vec::new(),
        }
    }

    /// Indexed symbol names that start with `prefix`, deduped and sorted, capped at
    /// [`MAX_COMPLETIONS`]. Streams the `MapCache` snapshot's outlines.
    fn complete_symbol_names(&self, prefix: &str) -> Vec<String> {
        let cache = self.state.shared.cache.load_full();
        // Owned rather than borrowed names: the outlines are streamed and dropped a chunk at a
        // time, so nothing may outlive the callback. The set is bounded by the repo's distinct
        // symbol names, and the result by `MAX_COMPLETIONS`.
        let mut names: BTreeSet<String> = BTreeSet::new();
        cache.for_each(|_, l1| {
            for symbol in &l1.symbols {
                if symbol.name.starts_with(prefix) {
                    names.insert(symbol.name.clone());
                }
            }
        });
        names.into_iter().take(MAX_COMPLETIONS).collect()
    }

    /// Indexed repo-relative file paths that start with `prefix`, capped at [`MAX_COMPLETIONS`].
    /// The file view is sorted, so keys are already ordered and prefix matches are contiguous.
    fn complete_file_paths(&self, prefix: &str) -> Vec<String> {
        let cache = self.state.shared.cache.load_full();
        cache
            .paths()
            .filter_map(|path| path.as_str())
            .filter(|path| path.starts_with(prefix))
            .take(MAX_COMPLETIONS)
            .map(str::to_owned)
            .collect()
    }
}
