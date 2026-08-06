---
name: bm
description: Ask basemind anything about the current codebase — outlines, refs, callers, git history, blame, diffs, docs, memory.
argument-hint: <question about the codebase>
---

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
| "Where is X defined?" | `code` mode `symbols` |
| "What calls X?" | `code` mode `references` (any name) or `callers` (specific definition) |
| "What's the shape of this file?" | `code` mode `outline` (add `l2: true`) |
| "What changed recently?" | `git` mode `recent`, `touching`, or `symbol_history` |
| "Who last touched this?" | `git` mode `blame` or `blame_symbol` |
| "Where's the churn?" | `git` mode `churn` |
| "Search PDFs/docs in the repo by meaning?" | `memory` mode `documents` |
| "Recall something remembered earlier?" | `memory` mode `get`, `list`, or `search` |
| "Remember this for later sessions?" | `memory` mode `put` (delete with `delete`) |
| "Refresh the index after editing code?" | `admin` mode `rescan` (`paths` limits scope) |

## Notes

- Answer with paths, line numbers, and signatures — read whole files only after a tool has
  located the exact span you need.

## See also

The `basemind` skill for the full tool-routing table and context-economy discipline.
