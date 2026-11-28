---
argument-hint: <question about the codebase>
description: Ask basemind anything about the current codebase — outlines, refs, callers, git history, blame, diffs, docs, memory.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:b705c53d39926e8769a862e2087eff2b58e60d089f719e77dfa693a99c902920
Source-Hash: blake3:cf13e28e976928ad07e04ec34e718a93ce3397449e6a6c38629c58faea0f9c4e
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
/bm who last touched crates/basemind-engine/src/scanner.rs?
```

Route the question to the tool that answers it directly:

| Example question | Tool |
|---|---|
| "Where is X defined?" | `code_symbols {"name":"MapCache"}` |
| "What calls X?" | `code_refs {"name":"process_file"}`; add `"path"` to pin one definition |
| "What's the shape of this file?" | `code_outline {"path":"crates/basemind-engine/src/scanner.rs"}` |
| "Show me that function" | `code_read {"path":"crates/basemind-engine/src/scanner.rs","symbol":"process_file"}` |
| "Where does this string appear?" | `code_grep {"pattern":"cache_root("}` (add `"regex":true` for a regex) |
| "Which file is named like X?" | `find_files {"query":"readme"}` |
| "What changed recently?" | `git_log {}`; `{"path":"crates/basemind-engine/src/scanner.rs"}` for one file; add `"symbol"` for one symbol |
| "Who last touched this?" | `git_blame {"path":"crates/basemind-engine/src/scanner.rs"}`; add `"symbol"` or `"line"` + `"end_line"` |
| "What's dirty, or what changed between revisions?" | `git_diff {}` / `git_diff {"path":"crates/basemind-engine/src/scanner.rs","from":"HEAD~1","to":"HEAD"}` |
| "Where's the churn?" | CLI only: `basemind git churn` |
| "Search PDFs/docs in the repo by meaning?" | `docs_search {"query":"how releases are versioned"}` |
| "Recall something remembered earlier?" | `memory_search {"query":"token rotation"}`, or `{"key":"…"}` / `{"prefix":"…"}` |
| "Remember this for later sessions?" | `memory_save {"key":"decision/x","value":"…"}` (delete: `basemind memory delete <key>`) |
| "A result says stale, or a new file is missing?" | `index_rescan {"paths":["crates/basemind-engine/src/scanner.rs"]}` |

## Notes

- The tools are always loaded: call them directly, no tool search needed. An unknown argument is
  rejected, and line numbers are 1-based.
- Answer with paths, line numbers, and signatures — read whole files only after a tool has
  located the exact span you need.

## See also

The `basemind` skill for the full tool-routing table and context-economy discipline.
