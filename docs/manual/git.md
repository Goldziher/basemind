# Git history

The git tools read history, blame and diffs without shelling out to `git`. History queries go
through a git-history index built after a scan (a first build runs in the background, see
`basemind manual indexing`), so commits touching a path come back in tens of microseconds; before
it is ready they walk git directly. They need the workspace to be a git
repository.

| Question | MCP call | CLI |
|---|---|---|
| What changed recently? | `git_log {}` | `basemind git log` |
| Which commits touched a file? | `git_log {"path":"src/store.rs"}` | `basemind git log --path src/store.rs` |
| Which commit mentions X? | `git_log {"query":"fix race"}` | `basemind git log --query "fix race"` |
| What did this author commit? | `git_log {"author":"ada"}` | `basemind git log --author ada` |
| When did a symbol change? | `git_log {"path":"src/store.rs","symbol":"open"}` | `basemind git log --path src/store.rs --symbol open` |
| Show one commit in full | `git_log {"rev":"HEAD~1"}` | `basemind git log --rev HEAD~1` |
| Who wrote these lines? | `git_blame {"path":"src/store.rs","line":120,"end_line":160}` | `basemind git blame src/store.rs --line 120 --end-line 160` |
| Who wrote this symbol? | `git_blame {"path":"src/store.rs","symbol":"open"}` | `basemind git blame src/store.rs --symbol open` |
| What is dirty in the working tree? | `git_diff {}` | `basemind git diff` |
| What did I change in a file? | `git_diff {"path":"src/store.rs"}` | `basemind git diff src/store.rs` |
| Diff a file between revisions | `git_diff {"path":"src/store.rs","from":"HEAD~1","to":"HEAD"}` | `basemind git diff src/store.rs --from HEAD~1 --to HEAD` |
| Which symbols were added or removed? | `git_diff {"path":"src/store.rs","from":"HEAD~5","structural":true}` | `basemind git diff src/store.rs --from HEAD~5 --structural` |

## Contracts

- `git_log` with no arguments is a recency window; `query` (commit message) and `author` (name and
  email) find a commit arbitrarily far back and combine with each other. `symbol` needs `path`.
- A symbol's history and a directory's history (a `path` ending in `/`) read one window of commits
  per page: an empty page with `truncated_reason: "span_limit"` and a `next_cursor` is not the end.
  Follow the cursor until it is absent. A directory page also reports `commits_reached`, how many
  commits (newest first) it and the pages before it have gone through, and `total_commits`, the
  commits in the history, when the git-history index serves it, so a caller can judge how far a
  rarely touched directory has left to go.
- `git_blame` locates a `symbol` in the file as it is at `rev` (default HEAD), so a symbol that
  exists only uncommitted is not found. `line` / `end_line` are 1-based and inclusive.
- A file's history, and a symbol's, comes from the git-history index when it holds HEAD, and a path
  no commit touches answers at once. Without it each page tests at most 10 000 commits of the live
  history: a page cut there says `truncated_reason: "span_limit"`, and its `next_cursor` goes on
  further back.
- `git_diff` with `path` diffs against HEAD by default (`from`) and the working tree (`to`). A diff
  stops at 200 hunks or 128 KiB and sets `truncated`; it has no cursor, so narrow `from` / `to`. A
  structural diff compares `from` with the indexed working tree and takes no `to`. Both kinds
  compare the working tree in the form git stores, as `git diff` does: line endings per `core.autocrlf` and
  `.gitattributes`, clean filters such as Git LFS run. A filter that fails is an error.
- A shallow clone sets `truncated` with `truncated_reason: "shallow_clone"`: an absent commit is
  then inconclusive. Answers taken while the clone is shallow are not cached, so the call after a
  `git fetch --unshallow` answers from the full history.

## CLI shorthands and CLI-only commands

`basemind git status`, `basemind git recent`, `basemind git touching <path>`,
`basemind git search <query>`, `basemind git show <rev>`, `basemind git symbol-history <path> <name>`,
`basemind git blame-symbol <path> <name>` and `basemind git diff-outline <path>` each stand for one
shape of the calls above. `basemind git churn` ranks the most frequently changed files and
`basemind git by-path <regex>` finds commits whose changed paths match a regex.
`basemind git hotspots` lists the files where change and size meet: the commits and authors in a
window (`--since`, default `90d`), lines, symbols, the longest function and importers, keeping the
files with two or more of these in the top 10% of the files changed in the window. It prints raw
numbers, no score; `--path` narrows it and `--limit` caps the rows (default 20). The window ends at
HEAD's commit time and counts the commits authored in it; a rebased or merged branch whose commits
keep older author dates is passed over, not taken for the window's start. None of the three has an
MCP tool.
