# Environment variables

Configuration that belongs to a repository lives in its `basemind.toml`. These variables are the
operator's: a repository cannot set them, so the grants among them cannot be turned on by a cloned
repository.

Every boolean variable, including the `BASEMIND_DOCUMENTS_*` switches and the status line's, reads
`1`, `true`, `yes` or `on` as true and `0`, `false`, `no` or `off` as false, in any case. A blank
value counts as unset. Any other value is an error that names the variable: the command refuses to
run, and the status line shows the error in place of its output. `BASEMIND_ALLOW_EXTRA_ROOTS` and
`BASEMIND_ALLOW_DEPENDENCY_ROOTS` are the exceptions: they also take a path list, so any other value
is read as that list, and a misspelling such as `tru` is a relative entry, ignored with a warning,
that grants nothing.

The scope says what reads a variable: `CLI` is any `basemind` command, `serve` included; `daemon`
is the background daemon, which inherits the environment of the command that started it;
`launcher` is the plugin's `mcp-launch.sh`; `hook` is a plugin hook or the status line; `wrapper`
is the npm or pip package.

## Locations

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_DATA_HOME` | the platform data directory | CLI, daemon | The data directory holding the cache, the daemon's files and telemetry. |
| `BASEMIND_COMMS_DIR` | under the data directory | CLI, daemon | The daemon's rendezvous directory. |

## Grants

Each grant is off unless true.

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_ALLOW_ANY_ROOT` | off | CLI, daemon | Index a directory that is neither a git repository nor holds a `basemind.toml`. A filesystem root is refused regardless. |
| `BASEMIND_ALLOW_EXTRA_ROOTS` | off | CLI, daemon | Honour `[scan] extra_roots`, directories outside the repository: true for every workspace, or a list of workspace roots. |
| `BASEMIND_ALLOW_DEPENDENCY_ROOTS` | off | CLI, daemon | Read `[documents.dependencies]` package roots outside the repository, in the same forms. |
| `BASEMIND_ALLOW_FOLLOW_SYMLINKS` | off | CLI, daemon | Honour `[scan] follow_symlinks = true`. |
| `BASEMIND_ALLOW_REPO_CREDENTIALS` | off | CLI, daemon | Let `[scan] floor_allow` index credential files (`.env`, `*.pem`, keys). |
| `BASEMIND_ALLOW_HTTP` | off | daemon | Bind the daemon's streamable-HTTP MCP front-end on loopback. |
| `BASEMIND_ALLOW_HTTP_REMOTE` | off | daemon | With `BASEMIND_ALLOW_HTTP`, let a non-loopback `BASEMIND_HTTP_ADDR` bind. The transport is plain HTTP, so the bearer token crosses the network in cleartext; prefer a loopback address reached through an SSH tunnel or a TLS reverse proxy. Without it such an address is refused. |

## Daemon and agents

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_NO_AUTOSPAWN` | off | CLI | Connect to a running daemon but never start one, for a daemon run by a service manager. |
| `BASEMIND_MAX_DAEMONS` | `8` | CLI, daemon | Ceiling on live daemons of a kind; a spawn over it is refused. |
| `BASEMIND_DAEMON_BINARY` | the running executable | CLI | The `basemind` binary a daemon is spawned from, for a host that is not itself `basemind`. |
| `BASEMIND_AGENT_ID` | derived from the session | CLI, hook | This checkout's agent id for messaging. |
| `BASEMIND_HOST` | set by the launcher | CLI, launcher, hook | The agent harness, such as `claude` or `codex`, part of the agent identity. |
| `BASEMIND_RELAY_REQUEST_TIMEOUT_SECS` | `180` | CLI | Deadline for one request through the stdio relay; `0` disables it. |
| `BASEMIND_COMMS_REQUEST_TIMEOUT_SECS` | `10` | CLI | Deadline for one daemon request. |
| `BASEMIND_COMMS_HANDSHAKE_TIMEOUT_SECS` | `30` | CLI | Deadline for connecting to the daemon and its handshake. |
| `BASEMIND_COMMS_IDLE_REAP_SECS` | `600` | daemon | Idle time after which a daemon with no connections and no work exits. |
| `BASEMIND_COMMS_IDLE_CHECK_SECS` | `60` | daemon | How often the daemon checks whether it is idle. |
| `BASEMIND_COMMS_BOOTSTRAP_SECS` | `120` | daemon | Time after which a daemon nothing ever connected to exits. |
| `BASEMIND_DAEMON_HEAVY_CONCURRENCY` | a quarter of the cores, 2 to 4 | daemon | Heavy calls (`index_rescan`, `code_search`, `docs_search`) run at once. |
| `BASEMIND_DAEMON_MEDIUM_CONCURRENCY` | half the cores, 2 to 8 | daemon | Whole-corpus reads (`code_grep`, `code_refs` importers and cycles, the CLI's impact, cycles, deadcode and overview) run at once. |
| `BASEMIND_DAEMON_QUEUE_WAIT_MS` | `2000` | daemon | How long a heavy or whole-corpus call waits for a slot before it is answered `server_busy`. |
| `BASEMIND_WARM_READ_STACKS` | `3` | daemon | Workspaces whose read stacks the daemon keeps loaded at once. |
| `BASEMIND_HTTP_ADDR` | `127.0.0.1:51786` | daemon | Address of the HTTP front-end `BASEMIND_ALLOW_HTTP` enables, `host:port`; a non-loopback address is warned about. |

The three admission settings apply to the whole daemon, which serves every workspace on the
machine, so they have no `basemind.toml` key. `index_status` reports each class's `limit`, `in_use`
and `queued` under `admission`.

## Network

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_OFFLINE` | off | CLI, daemon, launcher | Never download a grammar or a model, for this process and the daemon it starts; a missing one is an error naming it. Sets `HF_HUB_OFFLINE` for the models. The npm and pip installers run before basemind and still download an uncached binary. |
| `BASEMIND_RELEASE_BASE_URL` | GitHub | launcher | Download releases from a mirror instead of GitHub (`https://`, `file:///` or a loopback `http://` URL; layout in `docs/network.md`). |

`HF_ENDPOINT` redirects the Hugging Face model downloads (default `https://huggingface.co`) and
`TREE_SITTER_LANGUAGE_PACK_MANIFEST_URL` the grammar manifest (`https://` or `file://`), for a
mirror. Every outbound host, and what triggers each download, is listed in `docs/network.md`.

## Daemon ceilings

The daemon clamps every workspace's `basemind.toml` to these; a repository can lower a value but
not raise it. `BASEMIND_DAEMON_MIN_DEBOUNCE_MS` is a floor instead.

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_DAEMON_MAX_SCAN_THREADS` | `4` | daemon | `[resources] scan_threads`. |
| `BASEMIND_DAEMON_MAX_EMBED_THREADS` | `4` | daemon | `[resources] embed_threads`. |
| `BASEMIND_DAEMON_MAX_EMBED_BATCH` | `8` | daemon | `[resources] embed_batch_size`. |
| `BASEMIND_DAEMON_MAX_CONCURRENT_DOCUMENTS` | `4` | daemon | `[resources] max_concurrent_documents`. |
| `BASEMIND_DAEMON_MAX_FOOTPRINT_MB` | `3072` | daemon | `[resources] max_footprint_mb`, also when the file leaves it on `auto` or `off`. |
| `BASEMIND_DAEMON_MAX_MAP_CACHE_MB` | `1024` | daemon | `[resources] max_map_cache_mb`. |
| `BASEMIND_DAEMON_MAX_CANDIDATES` | `2000000` | daemon | `[scan] max_candidates`. |
| `BASEMIND_DAEMON_MAX_FILE_BYTES` | `67108864` | daemon | `[scan] max_file_bytes`. |
| `BASEMIND_DAEMON_MAX_DOCUMENT_BYTES` | `536870912` | daemon | `[documents] max_file_bytes`. |
| `BASEMIND_DAEMON_MAX_DOCUMENT_PAGES` | `5000` | daemon | `[documents] max_pages`. |
| `BASEMIND_DAEMON_MAX_EXTRACTION_SECS` | `1800` | daemon | `[documents] extraction_timeout_secs`. |
| `BASEMIND_DAEMON_MAX_CHUNKS_PER_DOCUMENT` | `20000` | daemon | `[documents] max_chunks_per_document`. |
| `BASEMIND_DAEMON_MIN_DEBOUNCE_MS` | `50` | daemon | `[watch] debounce_ms`. |

## Indexing and cache

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_GH_INDEX` | on | CLI, daemon | False turns the git-history index off; history queries then walk git. |
| `BASEMIND_GIT_DISCOVER_PARENTS` | off | CLI, daemon | True indexes an ancestor repository's history for a workspace that is a subdirectory of it. |
| `BASEMIND_GRAMMAR_OFFLINE` | off | CLI, daemon | Never download a grammar; a missing one is reported instead. Takes the values `BASEMIND_OFFLINE` takes. |
| `BASEMIND_PARSE_TIMEOUT_MS` | `5000` | CLI, daemon | Time after which parsing one file is abandoned. |
| `BASEMIND_MIN_FREE_MB` | `512` | CLI, daemon | Headroom a scan needs free on disk on top of its size estimate (1.5 times the index, plus the index rows, blobs and vectors of the files no file map lists yet), or it refuses. |
| `BASEMIND_CACHE_BUDGET_MB` | `min(20 GiB, (cache size + free space) / 2)` | CLI, daemon | Budget for the cache in MiB, enforced by eviction (`0`: unlimited). Unset: `[cache] budget_mb` in the user settings file (below), else the default; the free space is that of the cache's volume. `basemind cache stats` reports `budget_warning` when a set budget is larger than the cache plus that free space. |
| `BASEMIND_CACHE_GRACE_MINUTES` | `360` | CLI, daemon | A workspace built this recently is not evicted for the budget. |
| `BASEMIND_BLOB_GC_GRACE_SECS` | `21600` | CLI, daemon | Age under which `basemind cache gc` keeps an unreferenced blob. |
| `BASEMIND_INDEX_CACHE_BYTES` | sized from the index | CLI, daemon | Block cache of an opened index, in bytes. |
| `BASEMIND_GREP_BLOOM` | on | CLI, daemon | False makes `code_grep` read every file instead of skipping those its trigram filter rules out. |
| `BASEMIND_GIT_DELTA_CACHE_BYTES` | `8388608` | CLI, daemon | Git's delta-base cache per repository; `0` disables it. |
| `BASEMIND_BLAME_MAX_BYTES` | `1048576` | CLI, daemon | A larger file is not blamed. |
| `BASEMIND_BLAME_MAX_LINES` | `5000` | CLI, daemon | A file with more lines is not blamed. |
| `BASEMIND_GIT_CACHE_LOG_MAX_BYTES` | `268435456` | CLI, daemon | Disk budget of each git-cache directory; `0` disables eviction. |

## Document overrides

Each overrides one `[documents]` key of `basemind.toml` for `basemind scan` and `basemind serve`,
like the matching `--documents-…` flag. Unset, the file's value applies.

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_DOCUMENTS_ENABLED` | the file's | CLI | `enabled`. |
| `BASEMIND_DOCUMENTS_MAX_CHARACTERS` | the file's | CLI | `max_characters`. |
| `BASEMIND_DOCUMENTS_OVERLAP` | the file's | CLI | `overlap`. |
| `BASEMIND_DOCUMENTS_EMBEDDING_PRESET` | the file's | CLI | `embedding_preset`. |
| `BASEMIND_DOCUMENTS_EMBED` | the file's | CLI | `embed`. |
| `BASEMIND_DOCUMENTS_LANGUAGE_AUTO_DETECT` | the file's | CLI | `language.auto_detect`. |
| `BASEMIND_DOCUMENTS_LANGUAGE_MIN_CONFIDENCE` | the file's | CLI | `language.min_confidence`. |
| `BASEMIND_DOCUMENTS_LANGUAGE_DETECT_MULTIPLE` | the file's | CLI | `language.detect_multiple`. |
| `BASEMIND_DOCUMENTS_RERANKER_ENABLED` | the file's | CLI | `reranker.enabled`. |
| `BASEMIND_DOCUMENTS_RERANKER_PRESET` | the file's | CLI | `reranker.preset`. |
| `BASEMIND_DOCUMENTS_RERANKER_TOP_K` | the file's | CLI | `reranker.top_k`. |
| `BASEMIND_DOCUMENTS_KEYWORDS_ENABLED` | the file's | CLI | `keywords.enabled`. |
| `BASEMIND_DOCUMENTS_KEYWORDS_MAX_KEYWORDS` | the file's | CLI | `keywords.max_keywords`. |
| `BASEMIND_DOCUMENTS_KEYWORDS_MIN_SCORE` | the file's | CLI | `keywords.min_score`. |
| `BASEMIND_DOCUMENTS_NER_ENABLED` | the file's | CLI | `ner.enabled`. |
| `BASEMIND_DOCUMENTS_SUMMARIZATION_ENABLED` | the file's | CLI | `summarization.enabled`. |
| `BASEMIND_DOCUMENTS_SUMMARIZATION_STRATEGY` | the file's | CLI | `summarization.strategy`. |
| `BASEMIND_DOCUMENTS_SUMMARIZATION_MAX_TOKENS` | the file's | CLI | `summarization.max_tokens`. |

## Plugin and packages

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_BIN` | unset | launcher, wrapper | Run this `basemind` binary instead of the one the plugin or package installs, whatever its version. |
| `BASEMIND_BINARY` | unset | launcher, wrapper | Older name of `BASEMIND_BIN`, read when that is unset. |
| `BASEMIND_PRUNE_AFTER_DAYS` | `14` | launcher | Days the plugin launcher keeps an unused version, and the blob sweep an older version's blobs after their last write. |
| `BASEMIND_GUARD` | `nudge` | hook | What the pre-tool hook does with a search the indexed tools answer: `nudge` advises once, `redirect` refuses `Grep` and `Glob`, `off` does nothing. |
| `BASEMIND_HOOK_STATUS_TIMEOUT` | `20` | hook | Seconds the session-start hook waits for the index status before it gives up. |
| `BASEMIND_STATUSLINE` | `auto` | hook | Status-line layout: `full`, `compact`, `minimal`, or `auto` to pick by terminal width. |
| `BASEMIND_STATUSLINE_CONTEXT` | on | hook | False hides the status line's top line. |
| `BASEMIND_STATUSLINE_VERSION` | on | hook | False hides the plugin version on the status line. |

## Internal

Set by basemind for the processes it starts; an operator does not set them.

| Variable | Default | Scope | Effect |
|---|---|---|---|
| `BASEMIND_FORCE_VERSION` | unset | launcher | The version a re-executed launcher installs when the pinned release is not published yet. |
| `BASEMIND_AGENT_DEPTH` | unset | CLI | How deep in a chain of launched agents this process is. |
| `BASEMIND_LAUNCH_TOKEN` | unset | CLI | The credential a launched agent presents to its supervisor. |
| `BASEMIND_PARENT_AGENT_ID` | unset | CLI | The launching agent's id; removed before a daemon is spawned. |
| `BASEMIND_THREAD_ID` | unset | CLI | The launching thread's id; removed before a daemon is spawned. |

## Removed

`BASEMIND_DOCUMENTS_OUTPUT_FORMAT` and the `BASEMIND_LLM_*` variables (`BASEMIND_LLM_MODEL`,
`BASEMIND_LLM_API_KEY`, `BASEMIND_LLM_BASE_URL`, `BASEMIND_LLM_TEMPERATURE`,
`BASEMIND_LLM_TIMEOUT_SECS`, `BASEMIND_LLM_MAX_RETRIES`, `BASEMIND_LLM_MAX_TOKENS`) configure
settings that were removed. They are ignored, with a warning to unset them.

Logging follows `RUST_LOG`; `-v` and `-q` adjust it per command. `NO_COLOR` disables colour.

## User settings file

`settings.toml` in basemind's user configuration directory (`~/.config/basemind/` on Linux,
`~/Library/Application Support/basemind/` on macOS, under the account's home directory as the user
database names it) holds machine-wide settings every basemind process reads at start, the daemon an
MCP host spawns included. A variable naming the same setting wins over the file. A file that does not
parse, or holds an unknown key, is ignored with a warning.

```toml
[cache]
budget_mb = 8192   # as BASEMIND_CACHE_BUDGET_MB; 0 disables enforcement
```

A running daemon reads the file when it starts: restart it (`basemind comms stop`) after an edit.
