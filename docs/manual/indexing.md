# Indexing and freshness

`basemind scan` walks the repository (respecting `.gitignore`), parses code with tree-sitter,
extracts documents, and writes the result to a machine-global cache keyed by workspace: Linux
`~/.local/share/basemind/`, macOS `~/Library/Application Support/basemind/`, or
`BASEMIND_DATA_HOME`. Nothing is written into the repository. Content is stored by hash, so an
unchanged file is never extracted twice, across worktrees and repositories alike.

| Job | MCP call | CLI |
|---|---|---|
| Is there an index, and how fresh? | `index_status {}` | `basemind status` |
| Re-index files a result calls stale, or a new file | `index_rescan {"paths":["src/store.rs"]}` | `basemind rescan src/store.rs` |
| Re-index the whole tree (an empty, `degraded` or `rescan_needed` index only) | `index_rescan {}` | `basemind rescan` |
| Build the index from scratch | none | `basemind scan` |

`basemind scan --json` and `basemind rescan --json` print only the response `index_rescan` gives
(the counts, `elapsed_ms` and `root`), for a scan run here or forwarded to the daemon. A
working-tree `scan --json` that left vectors owed adds `embeddings`: `pending_documents`,
`pending_code`, `pending_policy` and `background` (`started`, `already_running`, `disabled` or
`unavailable`).

## Embeddings run in the background

A working-tree `basemind scan` never waits on the embedding model: it writes the code map, every
document's text and chunks and the keyword lane, then returns. The vectors it leaves owed (documents
without them, code chunks when `[code_search] embed = true`, rows a changed embed policy rebuilds)
are paid by a background pass, and the summary says so with an `embeddings: pending …` line.

- With a daemon running, the scan is forwarded to it and the daemon embeds afterwards.
- Otherwise the scan starts the hidden `embed-worker` command detached, at a lower CPU priority.
  It embeds the owed documents 32 at a time, then runs one embedding pass over the tree when code
  embedding is on, the embed policy changed or documents are left over. Its log is
  `embed-worker.log` in the workspace's cache directory.
- One worker runs per workspace; a scan that finds one running starts no other. The worker hands
  the index back to any other writer that needs it (a scan, a rescan, `basemind cache`) and
  resumes once that writer is done; it stops when `serve`, `watch` or the daemon takes the index.
- What is owed stays recorded in the index, so a worker that is killed loses nothing: the next
  `basemind scan`, a daemon or `embed-worker` run by hand in the repository (it finishes the pass
  in the foreground) continues where it stopped.
- `index_status` (`basemind status`) reports `embeddings: {done, total, eta_secs}` while vectors
  are owed, and `docs_search` answers from the keyword lane with a notice for the documents still
  without vectors. `BASEMIND_EMBED_BACKGROUND=false` stops the scan from starting the worker.
- `scan --staged` and `scan --rev` embed as they go, as `basemind rescan` does.

Besides the counts per language, `index_status` lists `large_files` (code files too long to read
whole) and `most_imported` (the five files most others import at runtime, absent when no import
resolver placed an edge). Its `admission` block shows how many heavy and whole-corpus calls the
server runs at once, how many run now and how many wait; a call that waits too long is answered
`server_busy` and should be retried (`basemind manual environment` sets the limits).

## Keeping it fresh

- The daemon behind `basemind serve` watches the tree and re-indexes changed files. Without a
  daemon, `basemind watch` does the same in the foreground, and `basemind hook install` adds a git
  pre-commit hook that runs `basemind scan --staged`.
- With a daemon running, `basemind scan` and `basemind rescan` are forwarded to it: the daemon is the
  only writer to the cache. A scan never starts a daemon.
- A file edited since the scan is re-read as it is now and the answer carries `stale`; a file
  deleted since the scan is an error naming `index_rescan`.
- `basemind scan --staged` indexes the git staging area and `basemind scan --rev <rev>` one
  revision, each as its own view; `--view` on a query command selects it.
- After a scan the git-history index is built or updated; `--no-git-history` skips it and
  `--rebuild-git-history` rebuilds it after a history rewrite.

## Large repositories: the inotify watch limit

On Linux the daemon watches each directory of each workspace with its own inotify watch, and the
kernel caps one user's watches across every process with `fs.inotify.max_user_watches`. Many
worktrees of a large repository can exhaust it: 20 worktrees of 20,000 directories each need about
400,000 watches. A directory the kernel refuses a watch is not watched, so its edits are not
indexed until a rescan. While any directory is unwatched, the daemon re-checks those subtrees every
30 seconds and re-extracts the files that changed, so edits there are indexed late rather than never;
it also retries arming them. `basemind doctor` has a `watches` row that counts the directories the
workspace needs and warns when that alone exceeds the limit.

`index_status` reports the watches the daemon holds under `watches.held`, and once the limit is hit
`watches.unwatched` (directories without a watch), `watches.limit` (the current value) and a
`watches.note`; the daemon log carries the same warning. Raise the limit and make it permanent:

```sh
sudo sysctl -w fs.inotify.max_user_watches=1048576
echo 'fs.inotify.max_user_watches=1048576' | sudo tee /etc/sysctl.d/90-basemind.conf
```

Then run `basemind comms stop` so the next call starts a daemon that watches every directory. Each
watch costs the kernel about 1 KiB, so a million watches is about 1 GiB at most. macOS and Windows
watch a tree with one recursive watch and have no such limit; `watches` is absent there.

## Notices

An answer given before the index is ready carries a `notice` instead of, or beside, its result:

| `state` | Meaning |
|---|---|
| `empty_index` | This workspace has no index yet: run `basemind scan`. |
| `warming_up` | An existing index is loading into memory. Retry shortly. |
| `building_index` | The first index for this workspace is being built. Retry shortly. |
| `rescanning` | A rescan is running; results are usable but may be a moment stale. |

## What a workspace root is

A root must be a git repository or a directory with a `basemind.toml` (at the root or under
`.config/`). Anything else is refused, so a host started in `/` or a home directory does not index
the whole disk; `basemind init` marks a directory you want indexed. Configuration lives in
`basemind.toml`: which files to scan, per-language overrides, the document tier and resource
ceilings.
