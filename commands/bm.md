---
name: bm
description: Ask basemind anything about the current codebase — outlines, refs, callers, git history, blame, diffs, docs, memory.
---

# bm — ask basemind anything about this codebase

Use the basemind MCP server to answer the user's question: $ARGUMENTS

Prefer basemind tools over reading files when navigating large or unfamiliar
codebases. Routing:

- "where is X defined?" → `search_symbols`
- "what calls X?" → `find_references` (any name) or `find_callers` (specific def)
- "shape of this file?" → `outline` (add `l2: true` for calls + docs)
- "what changed recently?" → `recent_changes`, `commits_touching`, `symbol_history`
- "who last touched this?" → `blame_file` / `blame_symbol`
- "where's the churn?" → `hot_files`
- "semantic search across PDFs/docs in the repo?" → `search_documents`
- "recall something the agent remembered earlier?" → `memory_get`
  / `memory_list` / `memory_search`
- "remember this for later sessions?" → `memory_put` (delete with `memory_delete`)
- "refresh the index after editing code?" → `rescan` (or `rescan { paths: [...] }`
  to limit to changed files)
