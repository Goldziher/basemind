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

## Locations

| Variable | Effect |
|---|---|
| `BASEMIND_DATA_HOME` | The data directory holding the cache, the daemon's files and telemetry. |
| `BASEMIND_COMMS_DIR` | The daemon's rendezvous directory (default: under the data directory). |

## Grants

| Variable | Effect |
|---|---|
| `BASEMIND_ALLOW_ANY_ROOT` | Index a directory that is neither a git repository nor holds a `basemind.toml`. A filesystem root is refused regardless. |
| `BASEMIND_ALLOW_EXTRA_ROOTS` | Honour `[scan] extra_roots`, directories outside the repository: true for every workspace, or a list of workspace roots. |
| `BASEMIND_ALLOW_DEPENDENCY_ROOTS` | Read `[documents.dependencies]` package roots outside the repository: true for every workspace, or a list of workspace roots. |
| `BASEMIND_ALLOW_FOLLOW_SYMLINKS` | Honour `[scan] follow_symlinks = true`. |
| `BASEMIND_ALLOW_REPO_CREDENTIALS` | Let `[scan] floor_allow` index credential files (`.env`, `*.pem`, keys). |
| `BASEMIND_ALLOW_HTTP` | Bind the daemon's streamable-HTTP MCP front-end on loopback. |

## Daemon and agents

| Variable | Effect |
|---|---|
| `BASEMIND_NO_AUTOSPAWN` | Connect to a running daemon but never start one, for a daemon run by a service manager. |
| `BASEMIND_MAX_DAEMONS` | Ceiling on live daemons of a kind; a spawn over it is refused (default 8). |
| `BASEMIND_AGENT_ID` | This checkout's agent id for messaging. |
| `BASEMIND_RELAY_REQUEST_TIMEOUT_SECS` | Deadline for one request through the stdio relay (default 180, `0` disables). |
| `BASEMIND_COMMS_REQUEST_TIMEOUT_SECS` | Deadline for one daemon request (default 10). |

## Indexing and cache

| Variable | Effect |
|---|---|
| `BASEMIND_GH_INDEX` | False turns the git-history index off; history queries then walk git. |
| `BASEMIND_GRAMMAR_OFFLINE` | Never download a grammar; a missing one is reported instead. |
| `BASEMIND_PARSE_TIMEOUT_MS` | Time after which parsing one file is abandoned. |
| `BASEMIND_MIN_FREE_MB` | Headroom, on top of 1.5 times the index size, a scan needs free on disk or it refuses. |
| `BASEMIND_CACHE_BUDGET_MB` | Byte budget for the cache, enforced by eviction (`0`: unlimited). |
| `BASEMIND_BLOB_GC_GRACE_SECS` | Age under which `basemind cache gc` keeps an unreferenced blob (default 6 h). |

Logging follows `RUST_LOG`; `-v` and `-q` adjust it per command. `NO_COLOR` disables colour.
