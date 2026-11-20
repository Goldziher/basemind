---
type: Decision
title: basemind usage
x-ai-rulez:
  kind: rule
  id: basemind-usage
  metadata:
    priority: high
---

# basemind usage

## basemind

**Use basemind first for every capability listed below.** Make the basemind MCP call before using a shell command, file read, or another search tool for the same question. Use the result to narrow any follow-up reads to the paths and lines you need.

The basemind tools are always loaded: call them directly, there is no tool search to run first. Arguments are exact (an unknown argument is rejected, not ignored), paths are repository-relative, and `line` / `end_line` are 1-based.

| Task | First call |
|---|---|
| Where is X defined | `code_symbols {"name":"parse_config"}` |
| What calls / implements / imports X | `code_refs {"name":"parse_config"}` (add `"relation":"implementations"` or `"importers"`) |
| Find a string, identifier or comment in code, markdown, config or data files | `code_grep {"pattern":"cache_root("}` (literal; add `"regex":true` for a regex) |
| Read one function or a line span | `code_read {"path":"src/store.rs","symbol":"open"}` |
| See what a code file contains without opening it | `code_outline {"path":"src/store.rs"}` |
| Recent commits, a file's history, or a commit search | `git_log {}` / `git_log {"path":"src/store.rs"}` / `git_log {"query":"fix race"}` |
| Who last changed a symbol or line span | `git_blame {"path":"src/store.rs","symbol":"open"}` |
| Working-tree status, or a file's diff | `git_diff {}` / `git_diff {"path":"src/store.rs"}` |
| Find a file by a fragment of its name or path | `find_files {"query":"readme"}` |
| Check the inbox, discover threads, or post to other agents (listed separately; needed only when collaborating) | `agent_inbox {}` / `agent_threads {"query":"…"}` / `agent_post {"subject":"…","body":"…"}` |
| Search documents by meaning (PDFs, Office, markdown, notebooks) | `docs_search {"query":"how releases are versioned"}` |
| Find code by what it does, then read the hit | `code_search {"query":"retry a failed upload with backoff"}` |

When a result is unexpectedly empty call `index_status {}`. The index follows your edits: if a result says `stale` or a new file is missing, call `index_rescan {"paths":["src/store.rs"]}` for that file (the one the result names, even if you did not edit it, and any you touched), and `index_rescan {}` only when `index_status` reports an empty index, `degraded` or `rescan_needed`.

Use a fallback when the capability is unavailable, the index is stale, or the input is outside its coverage. An empty indexed result only describes the indexed scope. For an exhaustive search, verify uncovered paths with an exact file search.

Use `docs_search` for semantic questions about notebooks and other indexed documents. For an exact literal in a notebook, search the file text directly.

### Setup & maintenance

- Install the basemind Claude Code plugin from its marketplace (`/plugin marketplace add Goldziher/basemind`, then install `basemind`).
- Keep basemind current: enable plugin auto-update, or update the binary regularly so the index format and tools stay in sync.
- Re-run `basemind init` (or `/bm-init`) after enabling new capabilities to refresh this block.
