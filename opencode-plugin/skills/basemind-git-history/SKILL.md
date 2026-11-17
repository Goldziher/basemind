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
Content-Hash: blake3:8eec70ca41681fc176f9cc3f123ab15208ffd7b998fa8f13ed711ad3f9499076
Source-Hash: blake3:4790ee5e237c434ff57929ff0428e9c5da0c4446ea6a6c82b509bbf114d292a7
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
  arbitrarily far back. `symbol` needs `path`. `limit` defaults to 20 and caps at 100, and
  `limit: 0` is refused. A `path` that no commit reachable from HEAD touches and that is not on
  disk is an error, for a file and for a directory.
- `git_log` with `path` + `symbol` reads one window of the commits touching the file per page.
  When older commits remain the page returns `next_cursor`, and says `truncated_reason: "window"`
  when it is short or empty: follow the cursor until it is absent to reach every change back to
  the introduction. A symbol the working tree no longer defines still has its history read. A
  name no commit inspected so far defines either is named in `unmatched` (with the names the file
  does define) on each empty page, and is an error on the last page.
- `git_log` with a directory `path` (one ending in `/`) pages the same way, 200 commits per page:
  an empty page with `truncated_reason: "window"` and a `next_cursor` is not the end of the
  history, and `unmatched` says when nothing has touched the directory so far and it is not on
  disk.
- A `git_log` cursor is refused when the git-history index came up (or stopped answering) since
  it was issued: the two order commits differently. Restart without `cursor`.
- `git_log` with `rev` holds the commit's file list to `max_tokens` and then sets `budgeted`, with
  `omitted` counting the files left out.
- `git_blame`: `line` and `end_line` are 1-based and inclusive, and `end_line` defaults to `line`.
  `rev` blames at a revision other than HEAD. A `symbol` is located in the file as it is at `rev`,
  not in the working tree, so its lines (and a `line` passed to pick one of several) are that
  revision's; a symbol that exists only uncommitted is "no symbol ... at HEAD". `limit` defaults to 100 hunks and the list is trimmed
  to `max_tokens` (default 6000), then sets `budgeted`. A hunk is `commit_sha`, `short_sha`, `line`, `end_line`, `source_line`, `author`,
  `author_time_unix` and `summary`; a symbol blame also carries the symbol's `line` / `end_line`.
- `git_diff`: `path` alone is that file's working-tree diff against HEAD; `from` defaults to HEAD
  and `to` to the working tree. The response has `from`, `to` (a sha, or `"working tree"`),
  `present_at_from`, `present_at_to` and `hunks`. A diff is cut at 200 hunks or 128 KiB of hunk
  text and then sets `truncated`, `truncated_reason` (`hunk_cap` or `byte_cap`) and `omitted` (the
  hunks left out); it has no cursor, so narrow it with a closer `from` / `to`. A structural diff
  compares `from` against the indexed working tree, takes no `to`, and returns `added`, `removed`
  and `common`, the count of unchanged symbols.
- `git_diff {}` lists at most 200 paths per status list, in path order. A cut status sets
  `truncated`, `truncated_reason: "path_cap"` and `omitted`; pass a directory `path` (one ending
  in `/`) for the status under that directory. That narrowed status lists every untracked file on
  its own and its `is_clean` is about the directory only. A directory that is not on disk and has
  no changed path under it is an error; so is a directory given without the trailing slash.
- All paths are repository-relative with forward-slash separators. Page with `next_cursor` ->
  `cursor`. A cursor continues only the call that returned it (same arguments apart from `limit`
  and `max_tokens`); on another query, or after HEAD moved, the call is an error: repeat it without
  `cursor`. A shallow clone sets `truncated` with `truncated_reason: "shallow_clone"`: absence is
  then inconclusive. Every cut uses the same words: `next_cursor` alone when a page ended at
  `limit`, `budgeted` when the token budget trimmed it, and `truncated` + `truncated_reason` for a
  cut the caller did not ask for, with `omitted` counting the items left out where known.
- `query` is matched against the commit message and `author` against the author name and email,
  each on its own field: `{"query":"review","author":"alice"}` is commits by alice that mention a
  review. A search answered before the git-history index is ready covers only the subjects and
  authors of the newest 10,000 commits and says so: `truncated` with
  `truncated_reason: "live_walk"`; retry shortly.
- When several definitions in the file share the name, `symbol` is an error listing each with its
  lines: add `line` (any line inside the one you mean).
- A binary file reports `binary: true` and no hunks, a file over 4 MiB `too_large: true`. A path
  that leaves the repository is refused.
- An argument a tool does not take is rejected with a did-you-mean and the accepted list.

For code structure see `basemind-code-search`; for document search see `basemind-documents`; for
agent coordination see `basemind-comms`.
