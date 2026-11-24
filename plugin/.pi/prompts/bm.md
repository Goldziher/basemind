---
argument-hint: <question about the codebase>
description: Ask basemind anything about the current codebase — outlines, refs, callers, git history, blame, diffs, docs, memory.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:a308775de78208b3fea4b033c74e064cdc600a4abc6a355d9cdb3ac0700b3eb3
Source-Hash: blake3:e320d694b0258ed06394743c75847d5990c0235672b6db89060dbf11b321b37d
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
| "Where is X defined?" | `code_symbols {"name":"MapCache"}` |
| "What calls X?" | `code_refs {"name":"process_file"}`; add `"path"` to pin one definition |
| "What's the shape of this file?" | `code_outline {"path":"src/scanner.rs"}` |
| "Show me that function" | `code_read {"path":"src/scanner.rs","symbol":"process_file"}` |
| "Where does this string appear?" | `code_grep {"pattern":"cache_root("}` (add `"regex":true` for a regex) |
| "Which file is named like X?" | `find_files {"query":"readme"}` |
| "What changed recently?" | `git_log {}`; `{"path":"src/scanner.rs"}` for one file; add `"symbol"` for one symbol |
| "Who last touched this?" | `git_blame {"path":"src/scanner.rs"}`; add `"symbol"` or `"line"` + `"end_line"` |
| "What's dirty, or what changed between revisions?" | `git_diff {}` / `git_diff {"path":"src/scanner.rs","from":"HEAD~1","to":"HEAD"}` |
| "Where's the churn?" | CLI only: `basemind git churn` |
| "Search PDFs/docs in the repo by meaning?" | `docs_search {"query":"how releases are versioned"}` |
| "Recall something remembered earlier?" | `memory_search {"query":"token rotation"}`, or `{"key":"…"}` / `{"prefix":"…"}` |
| "Remember this for later sessions?" | `memory_save {"key":"decision/x","value":"…"}` (delete: `basemind memory delete <key>`) |
| "A result says stale, or a new file is missing?" | `index_rescan {"paths":["src/scanner.rs"]}` |

## Notes

- The tools are always loaded: call them directly, no tool search needed. An unknown argument is
  rejected, and line numbers are 1-based.
- Answer with paths, line numbers, and signatures — read whole files only after a tool has
  located the exact span you need.

## See also

The `basemind` skill for the full tool-routing table and context-economy discipline.
