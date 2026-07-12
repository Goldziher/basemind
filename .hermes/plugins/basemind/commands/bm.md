---
name: bm
description: Ask basemind anything about the current codebase — outlines, refs, callers, git history, blame, diffs, docs, memory.
argument-hint: <question about the codebase>
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:8cdc39a2516f7fe0af441ddb5d47404d8716818b40f9967a10e907690e537e38
Source-Hash: blake3:ca37d0bd1accfd4a3156158a1f47fd4568c00994d83f95330f41124a857f34dc
Schema-Version: v1
-->

# bm — ask basemind anything about this codebase

Answer the user's question using the basemind MCP server instead of reading files or shelling
out to grep/git.

## When to use

Invoke with a natural-language question about this repo's code, history, or documents —
`/bm <question>`. Use it instead of manually picking a tool when you just want an answer.

## How to use

```text
/bm where is MapCache defined?
/bm what calls process_file?
/bm who last touched src/scanner.rs?
```

Route the question to the tool that answers it directly:

| Example question | Tool |
|---|---|
| "Where is X defined?" | `search_symbols` |
| "What calls X?" | `find_references` (any name) or `find_callers` (specific def) |
| "What's the shape of this file?" | `outline` (add `l2: true` for calls + docs) |
| "What changed recently?" | `recent_changes`, `commits_touching`, `symbol_history` |
| "Who last touched this?" | `blame_file` / `blame_symbol` |
| "Where's the churn?" | `hot_files` |
| "Search PDFs/docs in the repo by meaning?" | `search_documents` |
| "Recall something remembered earlier?" | `memory_get` / `memory_list` / `memory_search` |
| "Remember this for later sessions?" | `memory_put` (delete with `memory_delete`) |
| "Refresh the index after editing code?" | `rescan` (pass `paths: [...]` to limit the scope) |

## Notes

- Answer with paths, line numbers, and signatures — read whole files only after a tool has
  located the exact span you need.

## See also

The `basemind` skill for the full tool-routing table and context-economy discipline.
