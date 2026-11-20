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
| Re-index the whole tree (an empty or degraded index only) | `index_rescan {}` | `basemind rescan` |
| Build the index from scratch | none | `basemind scan` |

Besides the counts per language, `index_status` lists `large_files` (code files too long to read
whole) and `most_imported` (the five files most others import at runtime, absent when no import
resolver placed an edge).

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
indexed until a rescan.

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
