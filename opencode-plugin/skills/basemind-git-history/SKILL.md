---
name: basemind-git-history
description: >-
  Explore git history without shelling out to git — recent commits, commits touching a path,
  per-line and per-symbol blame, structural diffs across revisions, churn ranking, and a symbol's
  history over time. Reach for it whenever the user asks "what changed recently", "who last touched
  this", "when did this symbol change", "what's the diff between these revs", or "where's the churn".
---

# basemind-git-history — git intelligence over the index

basemind indexes git history and resolves blame and diffs at **symbol** resolution, backed by a
`gix` history index. History queries return commits, authors, paths, and line/symbol attributions —
structured and capped — for a fraction of the cost of parsing `git log` / `git blame` output.

**basemind first, naked git fallback.** Prefer these tools over shelling out to `git log`,
`git blame`, or `git diff`. Drop to raw git only when no tool covers the question (e.g. staging,
rebasing, anything that mutates history).

## Tool routing

| Question | MCP tool | CLI |
|---|---|---|
| "What changed recently?" | `git` mode `recent` | `basemind git recent [--limit N]` |
| "Which commits touched path P?" | `git` mode `touching` | `basemind git touching P` |
| "Path-filtered commit log?" | `git` mode `by_path` | `basemind git by-path P` |
| "When did symbol X last change?" | `git` mode `symbol_history` | `basemind git symbol-history F name` |
| "Who wrote this line?" | `git` mode `blame` | `basemind git blame F` |
| "Who wrote this symbol / when did its body change?" | `git` mode `blame_symbol` | `basemind git blame-symbol F name` |
| "Diff a file between revs?" | `git` mode `diff` | `basemind git diff F old new` |
| "What symbols did a branch add/remove?" | `git` mode `diff_outline` | `basemind git diff-outline F --rev old` |
| "Where's the churn?" | `git` mode `churn` | `basemind git churn [--top-k N]` |
| "What's dirty in the working tree?" | `git` mode `status` | `basemind git status` |
| "What's HEAD / branch / origin?" | `admin` mode `repo` | `basemind admin repo` |
| "Full-text search commit messages + authors?" | `git` mode `search` | `basemind git search "query"` |

## Examples

```text
git { mode: "recent", limit: 5 }
→ 612df7e chore(release): v0.15.0
  1779b99 fix(git-history,serve): address code-review findings
  ...

git { mode: "blame_symbol", path: "src/scanner.rs", name: "process_file" }
→ last touched by <author> in <commit> — body hash changed at HEAD~7

git { mode: "diff_outline", path: "src/mcp/tools.rs", rev: "HEAD~5" }
→ + git (function)  - old_helper (function)
```

## Notes

- Git tools require `basemind serve` to be running **inside a git repository**. Outside a git repo
  they return a clear error.
- History queries are indexed: `git` mode `touching` and its siblings resolve in tens of microseconds vs a
  live walk. The index auto-builds on first use and is a fraction of the size of `.git`.
- All paths are repository-relative with forward-slash separators. Lists are capped
  (`limit`, default 100, max 1000).

For code structure see `basemind-code-search`; for document RAG and semantic search see
`basemind-documents`; for agent coordination see `basemind-comms`.
