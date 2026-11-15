---
description: Explore git history without shelling out to git — recent commits, commits touching a path, per-line and per-symbol blame, structural diffs across revisions, churn ranking, and a symbol's history over time. Reach for it whenever the user asks "what changed recently", "who last touched this", "when did this symbol change", "what's the diff between these revs", or "where's the churn".
name: basemind-git-history
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:bddcade9f88271389cf0fb50ce9f64f31092ac4ab4354fdd0141e61824fab5c5
Source-Hash: blake3:cedfa4bf6ba470275f00d362ebb8180f60f5812f1912dd7ea32768d91fd82ba9
Schema-Version: v1
-->

# basemind-git-history — git history over the index

basemind indexes git history and resolves blame and diffs at **symbol** resolution, backed by a
`gix` history index. Three tools cover it: `git_log`, `git_blame` and `git_diff`. They return
commits, authors, paths, and line/symbol attributions, structured and capped, for a fraction of
the cost of parsing `git log` / `git blame` output.

The tools are always loaded: call them directly, no tool search needed. Prefer them over shelling
out to `git log`, `git blame`, `git status` or `git diff`. Drop to raw git only when no tool covers
the question (staging, rebasing, anything that mutates history).

## Tool routing

| Question | MCP call | CLI |
|---|---|---|
| "What changed recently?" | `git_log {}` | `basemind git log` |
| "Which commits touched file F?" | `git_log {"path":"src/store.rs","limit":5}` | `basemind git log --path src/store.rs --limit 5` |
| "Which commits touched a directory?" | `git_log {"path":"src/mcp/"}` | `basemind git log --path src/mcp/` |
| "Which commit mentions X?" | `git_log {"query":"fix race"}` | `basemind git log --query "fix race"` |
| "What did this author commit?" | `git_log {"author":"ada"}` | `basemind git log --author ada` |
| "When did symbol X change?" | `git_log {"path":"src/store.rs","symbol":"open"}` | `basemind git log --path src/store.rs --symbol open` |
| "Show one commit in full" | `git_log {"rev":"HEAD~1"}` | `basemind git log --rev HEAD~1` |
| "Who wrote this file's lines?" | `git_blame {"path":"src/store.rs"}` | `basemind git blame src/store.rs` |
| "Who wrote these lines?" | `git_blame {"path":"src/store.rs","line":120,"end_line":160}` | `basemind git blame src/store.rs --line 120 --end-line 160` |
| "Who wrote this symbol?" | `git_blame {"path":"src/store.rs","symbol":"open"}` | `basemind git blame src/store.rs --symbol open` |
| "What's dirty in the working tree?" | `git_diff {}` | `basemind git diff` |
| "What did I change in file F?" | `git_diff {"path":"src/store.rs"}` | `basemind git diff src/store.rs` |
| "Diff a file between revisions?" | `git_diff {"path":"src/store.rs","from":"HEAD~1","to":"HEAD"}` | `basemind git diff src/store.rs --from HEAD~1 --to HEAD` |
| "What symbols were added or removed?" | `git_diff {"path":"src/store.rs","from":"HEAD~5","structural":true}` | `basemind git diff src/store.rs --from HEAD~5 --structural` |
| "What's HEAD / branch?" | `index_status {}` | `basemind status` |
| "Where's the churn?" | CLI only | `basemind git churn` |
| "Commits whose changed paths match a regex?" | CLI only | `basemind git by-path <regex>` |

The CLI also keeps shorthands: `git recent`, `git touching <path>`, `git search <query>`,
`git show <rev>`, `git symbol-history <path> <name>`, `git blame-symbol <path> <name>`,
`git status`, `git diff-outline <path>`.

## Examples

```text
git_log {"limit":5}
→ 612df7e chore(release): v0.15.0
  1779b99 fix(git-history,serve): address code-review findings
  ...

git_blame {"path":"src/scanner.rs","symbol":"process_file"}
→ per-line authorship of that function's span

git_diff {"path":"src/mcp/tools.rs","from":"HEAD~5","structural":true}
→ + search_history (function)  - old_helper (function)
```

## Notes

- The git tools require `basemind serve` to be running **inside a git repository**. Outside one
  they answer with an error result saying so.
- History queries are indexed: commits touching a path resolve in tens of microseconds vs a live
  walk. The index auto-builds on first use and is a fraction of the size of `.git`.
- `git_log` with no arguments is a recency window; use `query` or `author` to find a commit
  arbitrarily far back. `symbol` needs `path`. `limit` defaults to 20 and caps at 100.
- `git_blame`: `line` needs `end_line`; both are 1-based and inclusive. `rev` blames at a revision
  other than HEAD. A hunk is `commit_sha`, `short_sha`, `line`, `end_line`, `source_line`, `author`,
  `author_time_unix` and `summary`; a symbol blame also carries the symbol's `line` / `end_line`.
- `git_diff`: `path` alone is that file's working-tree diff against HEAD; `from` defaults to HEAD
  and `to` to the working tree. The response has `from`, `to` (a sha, or `"working tree"`),
  `present_at_from`, `present_at_to` and `hunks`. A structural diff compares `from` against the
  indexed working tree and takes no `to`.
- All paths are repository-relative with forward-slash separators. Page with `next_cursor` ->
  `cursor`; a moved HEAD invalidates a cursor (`cursor_invalidated`). A shallow clone sets
  `truncated`: absence is then inconclusive.
- An argument a tool does not take is rejected with a did-you-mean and the accepted list.

For code structure see `basemind-code-search`; for document search see `basemind-documents`; for
agent coordination see `basemind-comms`.
