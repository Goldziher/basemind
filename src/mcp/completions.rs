//! Argument completion (`completion/complete`) for prompt arguments.
//!
//! MCP completion is scoped to a *reference* — a prompt or a resource template — not to arbitrary
//! tool arguments. basemind backs the typed arguments of its [`super::prompts`] templates from the
//! in-RAM code map: the `trace-symbol` prompt's `symbol` argument completes against indexed symbol
//! names, and the `explain-file` prompt's `path` argument completes against indexed file paths.
//! Both sources are the `MapCache.by_path` snapshot already held in RAM, so completion is a pure
//! prefix scan with no store lock and no disk I/O.

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
            // completes only prompt arguments, so any future reference kind yields no candidates.
            _ => Vec::new(),
        };
        // `CompletionInfo::new` only errors when `values.len() > 100`; every source caps at
        // MAX_COMPLETIONS, so this never errors — fall back to an empty set if it somehow does.
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
    /// [`MAX_COMPLETIONS`]. Pure in-RAM scan of the `MapCache` snapshot.
    fn complete_symbol_names(&self, prefix: &str) -> Vec<String> {
        let cache = self.state.cache.load_full();
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for l1 in cache.by_path.values() {
            for symbol in &l1.symbols {
                if symbol.name.starts_with(prefix) {
                    names.insert(symbol.name.as_str());
                }
            }
        }
        names.into_iter().take(MAX_COMPLETIONS).map(str::to_owned).collect()
    }

    /// Indexed repo-relative file paths that start with `prefix`, capped at [`MAX_COMPLETIONS`].
    /// `by_path` is a `BTreeMap`, so keys are already sorted and prefix matches are contiguous.
    fn complete_file_paths(&self, prefix: &str) -> Vec<String> {
        let cache = self.state.cache.load_full();
        cache
            .by_path
            .keys()
            .filter_map(|path| path.as_str())
            .filter(|path| path.starts_with(prefix))
            .take(MAX_COMPLETIONS)
            .map(str::to_owned)
            .collect()
    }
}
