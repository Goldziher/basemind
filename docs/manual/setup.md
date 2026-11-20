# Setup and administration

## Onboarding

`basemind init` writes a commented `basemind.toml` when there is none, lets you pick which
capabilities to advertise (`--with`, `--without`, or every one with `--yes`), and injects a
"prefer basemind over grep, read and git" rules block into a rules file. It is re-runnable: the
block is delimited and replaced in place. `--print` shows what it would change and writes nothing;
`--rules-target` picks the file and `--no-rules` skips it.

## The MCP server

`basemind serve` is what an MCP host launches over stdio: it ensures the daemon is up and relays
to it, so every session on a repository, and on its worktrees, shares one index. `basemind daemon ensure`
starts the daemon with its streamable-HTTP transport and prints the MCP URL, for hosts that dial
HTTP; the HTTP front-end binds only when `BASEMIND_ALLOW_HTTP` is set. `basemind statusline`
prints a one-line summary of the daemon's active workspaces for a shell prompt.

## Grammars, hooks and the cache

| Command | Job |
|---|---|
| `basemind lang list` | Installed tree-sitter grammars and where they live. |
| `basemind lang install` | Download every supported grammar now (otherwise each downloads on first use). |
| `basemind lang clean` | Delete the grammar cache. |
| `basemind hook install` | Add a git pre-commit hook running `basemind scan --staged`; it never blocks a commit. |
| `basemind cache stats` | Disk footprint per component and process memory. |
| `basemind cache gc` | Reap the caches of deleted worktrees and their groups, then remove blobs no workspace on the machine references (`--dry-run` only counts). |
| `basemind cache clear` | Clear one component (`--component`); everything but `git-cache` needs `--yes` off a terminal. |

## Admin and reports

| Command | Job |
|---|---|
| `basemind doctor` | Check root, config, index, grammars, hook and daemon; exits 1 when a check fails. |
| `basemind admin repo` | Index health plus branch and HEAD, the same as `basemind status`. |
| `basemind admin telemetry` | What was queried and the tokens saved, over `--window`. |
| `basemind admin eval` | Score retrieval quality against a JSONL task file (`benchmarks/eval/`). |
| `basemind admin tokens` | Count tokens in stdin with the o200k tokenizer. |
| `basemind sessions` | Count basemind calls against shell searches in the local agent transcripts, subagents included; a removed worktree's sessions are not counted. |
| `basemind completions <shell>` | Print a shell completion script. |
| `basemind man` | Print the man page (roff). |
| `basemind manual [topic]` | Print this manual. |

## Feature builds

The release binary carries every feature. A source build picks them with Cargo features:
`code-search` for `code_search`, `documents` for `docs_search`, `memory` for `memory_save` and
`memory_search`, `comms` for the daemon, `basemind serve` and agent messaging; `full` enables them
all. A tool whose feature is missing still appears and answers with an error naming the feature.
