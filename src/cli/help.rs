//! Long help (`--help`, after the options) of every command outside `basemind code` and
//! `basemind git` (those are in [`super::help_code`]).
//!
//! Each text has the same four parts: `When to use:`, `Returns:`, an `Example:` command line and
//! `Next:`. `tests/cli_parity/help.rs` walks the command tree and fails on a command without them
//! or on an example or a quoted command the CLI does not accept. `-h` stays the one-line summary.

/// `basemind init`.
pub const INIT: &str = "\
When to use: onboarding a repository, or refreshing its basemind setup after an upgrade.

Returns: what it wrote: a commented `basemind.toml` when there is none, the capabilities to
advertise (`--with`, `--without`, `--yes` for all), and a delimited \"prefer basemind\" rules block,
replaced in place on a rerun. It offers to gitignore the `.local` files it writes. `--print` shows
the changes and writes nothing.

Example:
  basemind init --print

Next: `basemind scan` to build the index.";

/// `basemind scan`.
pub const SCAN: &str = "\
When to use: building the index for the first time, or a view of the staging area (`--staged`)
or of one revision (`--rev`).

Returns: one summary line (files scanned, updated, reused, unchanged, failed, skipped, removed),
then the git-history index build. A working-tree scan does not embed: an `embeddings: pending`
line counts what a background pass (the daemon, else a detached `embed-worker` process) embeds
next. With a daemon running, a working-tree scan is forwarded to it; a scan never starts a daemon.
A newer minor release rebuilds the index on its first scan.

Example:
  basemind scan

Next: `basemind status` to check the index.";

/// `basemind rescan`.
pub const RESCAN: &str = "\
When to use: after editing files, when no daemon is watching the tree, or to refresh a stale
index without starting one.

Returns: the same summary line as `scan`, for the given paths only or, with none, for the whole
working tree, skipping unchanged files. With a daemon running, the rescan is forwarded to it.

Example:
  basemind rescan src/store.rs

Next: `basemind code outline <path>` on a rescanned file.";

/// `basemind watch`.
pub const WATCH: &str = "\
When to use: keeping the index current in the foreground when no daemon runs. The daemon behind
`basemind serve` already watches the tree, and only one writer may hold a workspace.

Returns: the initial scan's summary, then one batch line per set of changed files, until
interrupted.

Example:
  basemind watch

Next: `basemind status` from another shell to see the index.";

/// `basemind doctor`.
pub const DOCTOR: &str = "\
When to use: basemind is not working, or before reporting a problem.

Returns: one verdict per check: the workspace root, the config, the index, the grammars, the git
hook and the daemon. Exits 1 when a check fails, naming it.

Example:
  basemind doctor

Next: `basemind manual troubleshooting` for the fix of a failed check.";

/// `basemind status`.
pub const STATUS: &str = "\
When to use: checking that an index exists and how fresh it is.

Returns: file counts per language, the scan age, the code files too long to read whole
(`large_files`) and the files most others import (`most_imported`). The same as the MCP tool
`index_status`.

Example:
  basemind status

Next: `basemind scan` when there is no index, `basemind overview` for the repository's shape.";

/// `basemind overview`.
pub const OVERVIEW: &str = "\
When to use: first contact with a repository.

Returns: its languages, top-level layout, build manifests and entry points, the largest and most
imported files, HTTP routes, naming conventions and recent commits. `--sections` picks sections
and `--limit` the rows per list (default 10).

Example:
  basemind overview --sections languages,entry_points

Next: `basemind code explain <path>` on an entry point.";

/// `basemind completions`.
pub const COMPLETIONS: &str = "\
When to use: installing shell completions.

Returns: the completion script for the shell, on stdout.

Example:
  basemind completions zsh > _basemind

Next: put the file where your shell loads completions from.";

/// `basemind man`.
pub const MAN: &str = "\
When to use: installing the man page.

Returns: the man page in roff, on stdout.

Example:
  basemind man > basemind.1

Next: `basemind manual` for the topic pages.";

/// `basemind manual`.
pub const MANUAL: &str = "\
When to use: you need the offline reference: what each command and MCP tool does and when to
use it. It needs no index and no daemon.

Returns: the overview and the list of topics, or one topic's page.

Example:
  basemind manual code

Next: `basemind manual troubleshooting` when something fails.";

/// `basemind serve`.
pub const SERVE: &str = "\
When to use: as the command an MCP host launches over stdio; you rarely run it by hand.

Returns: an MCP session on stdin/stdout. It ensures the machine's daemon is up and relays to it,
so every session on a repository and its worktrees shares one index. The daemon keeps the
environment of the process that started it.

Example:
  basemind serve

Next: `basemind daemon ensure` for a host that dials HTTP instead.";

/// `basemind statusline`.
pub const STATUSLINE: &str = "\
When to use: in a shell or agent statusline.

Returns: one compact line: with `--root`, that workspace's file count, scan age and recent tool
calls; without it, the daemon's hot workspaces. Prints nothing and exits 0 when there is nothing
to report.

Example:
  basemind statusline --root .

Next: `basemind status` for the full index report.";

/// `basemind sessions`.
pub const SESSIONS: &str = "\
When to use: measuring how agents in this repository use basemind against shell searches and
whole-file reads.

Returns: per session of the local Claude Code and Codex transcripts of this repository and its
worktrees, subagents under their parent, the counts of basemind calls, shell searches and file
reads. Counts and paths only; it reads the transcripts in place and sends nothing anywhere.

Example:
  basemind sessions --since 2026-11-01

Next: `basemind admin telemetry` for the tool calls basemind recorded.";

/// `basemind memory put`.
pub const MEMORY_PUT: &str = "\
When to use: recording a decision, a convention or a finding that a later session or another
agent should know.

Returns: the key, its timestamps and whether the note can be found by meaning (`embedded`). An
existing key is replaced. Notes are shared by every agent of the repository unless `--individual`.

Example:
  basemind memory put decision/auth-tokens \"Tokens rotate every 24h.\" --tag auth

Next: `basemind memory search <query>` to recall it.";

/// `basemind memory search`.
pub const MEMORY_SEARCH: &str = "\
When to use: recalling notes by meaning when you do not know their key.

Returns: the closest notes with their keys, tags and values; `--tag` filters them.

Example:
  basemind memory search \"token rotation\"

Next: `basemind memory get <key>` for one note whole.";

/// `basemind memory get`.
pub const MEMORY_GET: &str = "\
When to use: you know a note's key.

Returns: that note, value whole.

Example:
  basemind memory get decision/auth-tokens

Next: `basemind memory put <key> <value>` to update it.";

/// `basemind memory list`.
pub const MEMORY_LIST: &str = "\
When to use: browsing notes under a key prefix or with a tag.

Returns: the notes' keys with the first 200 characters of each value, paged with `next_cursor`.

Example:
  basemind memory list --prefix decision/

Next: `basemind memory get <key>` for a note whole.";

/// `basemind memory delete`.
pub const MEMORY_DELETE: &str = "\
When to use: a note is wrong or no longer applies.

Returns: whether a note was deleted.

Example:
  basemind memory delete decision/auth-tokens

Next: `basemind memory list` to review what remains.";

/// `basemind docs search`.
pub const DOCS_SEARCH: &str = "\
When to use: you are looking for a topic in prose, data or config files: Markdown, PDF, Office,
HTML, email, JSON, YAML, TOML. The code tools do not cover them.

Returns: ranked passages, each cited with its path and its pages, section or lines. `--format`,
`--path` (a glob), `--mime-type`, `--keywords` and `--entity` filter before ranking. Chunks of one
document that touch are one hit; `--per-doc` (default 2, 0 for no cap) puts other documents ahead of
a document's later hits; `--context` adds the chunk before and after each hit. Needs a build with
the `documents` feature and a scan.

Example:
  basemind docs search \"how releases are versioned\" --format markdown

Next: `basemind code read <path> --line <n>` on a cited text file.";

/// `basemind memory clear`.
pub const MEMORY_CLEAR: &str = "\
When to use: you want to start this repository's memory over, or remove every note before
deleting the repository's data.

Returns: how many notes were removed. It asks for confirmation on a terminal and needs `--yes`
anywhere else. Notes of every tier and agent go, with their vectors.

Example:
  basemind memory clear --yes

Next: `basemind memory list` to confirm nothing is left.";

/// `basemind upgrade`.
pub const UPGRADE: &str = "\
When to use: before installing a release that bumps the release minor, to see what the next scan
rebuilds.

Returns: per repository group and workspace, what is rebuilt, the bytes that frees and needs, and a
scan estimate. It reads only; nothing is converted until each workspace's next scan.

Example:
  basemind upgrade --plan

Next: `basemind scan` in each workspace once the new binary is installed.";

/// `basemind admin repo`.
pub const ADMIN_REPO: &str = "\
When to use: you need the index health together with the branch and HEAD.

Returns: the same result as `basemind status`, the MCP tool `index_status`, which carries the
branch and HEAD sha.

Example:
  basemind admin repo

Next: `basemind scan` when the index is behind HEAD.";

/// `basemind admin telemetry`.
pub const ADMIN_TELEMETRY: &str = "\
When to use: reviewing what was queried and how many tokens basemind saved.

Returns: the calls, response bytes and estimated tokens saved over `--window` (`today`, `1h`,
`24h` or `all`), per tool and per baseline, and the latest calls; `--tool` narrows it to one tool.

Example:
  basemind admin telemetry --window 24h

Next: `basemind sessions` for the agents' shell searches next to their basemind calls.";

/// `basemind admin tokens`.
pub const ADMIN_TOKENS: &str = "\
When to use: scoring text in a script with the same tokenizer as basemind's budgets.

Returns: the o200k token count of stdin, as a bare integer. Needs the `tokenizer` feature, which
`documents` includes.

Example:
  basemind admin tokens --stdin < README.md

Next: `basemind admin eval` for a full retrieval benchmark.";

/// `basemind admin eval`.
pub const ADMIN_EVAL: &str = "\
When to use: measuring retrieval quality and cost, or gating a change on them in CI.

Returns: per task of the JSONL file, the score against gold (precision, recall, F1, hit@k, MRR,
nDCG), latency and response tokens next to a grep/read baseline; `--report` and `--markdown`
write the aggregate, and `--baseline` exits non-zero on a regression.

Example:
  basemind admin eval --tasks tasks.jsonl --report report.json

Next: `basemind admin eval --tasks <file> --baseline <report>` to compare with a stored report.";

/// `basemind cache gc`.
pub const CACHE_GC: &str = "\
When to use: reclaiming disk from extraction blobs no workspace references any more.

Returns: the blobs inspected and removed and the bytes freed; `--dry-run` only counts. Blobs
younger than 6 h are kept (`BASEMIND_BLOB_GC_GRACE_SECS`).

Example:
  basemind cache gc --dry-run

Next: `basemind cache stats` to see the footprint.";

/// `basemind cache stats`.
pub const CACHE_STATS: &str = "\
When to use: checking how much disk and memory basemind uses.

Returns: the on-disk size per cache component and the blob accounting of the machine-global cache.

Example:
  basemind cache stats

Next: `basemind cache gc` or `basemind cache clear --component <name>` to reclaim space.";

/// `basemind cache clear`.
pub const CACHE_CLEAR: &str = "\
When to use: dropping one cache component to rebuild it, for example after a downgrade.

Returns: what was removed. `--component` is `blobs`, `views`, `lance`, `git-cache`, `telemetry`,
`all` or `views:<name>` (default `git-cache`). All but `git-cache` ask for confirmation on a
terminal and need `--yes` otherwise; `blobs` is shared by every workspace on the machine. Refuses
(exit 3) while a writer holds the workspace.

Example:
  basemind cache clear --component views --yes

Next: `basemind scan` to rebuild what was cleared.";

/// `basemind hook install`.
pub const HOOK_INSTALL: &str = "\
When to use: keeping the index of the staging area current without a daemon.

Returns: the path of the pre-commit hook it wrote, which runs `basemind scan --staged` and never
blocks a commit. It honours `core.hooksPath` and linked worktrees; `--force` replaces a hook
basemind did not write, keeping it as `pre-commit.bak`.

Example:
  basemind hook install

Next: `basemind doctor` to confirm the hook is found.";

/// `basemind lang list`.
pub const LANG_LIST: &str = "\
When to use: checking which tree-sitter grammars are installed.

Returns: the installed grammars and the directory they live in.

Example:
  basemind lang list

Next: `basemind lang install` to download the rest.";

/// `basemind lang install`.
pub const LANG_INSTALL: &str = "\
When to use: before working offline or in CI, so no grammar downloads on first use.

Returns: the grammars downloaded: every supported one plus any `[languages.<name>] preload = true`
of `basemind.toml`; a no-op for those already cached.

Example:
  basemind lang install

Next: `basemind lang list` to confirm.";

/// `basemind lang clean`.
pub const LANG_CLEAN: &str = "\
When to use: a grammar is corrupt, or to reclaim its disk.

Returns: the removed grammar cache. Grammars download again on next use. With BASEMIND_OFFLINE or
BASEMIND_GRAMMAR_OFFLINE set they cannot, so the command refuses (exit 2) unless `--yes` is passed.

Example:
  basemind lang clean
  basemind lang clean --yes

Next: `basemind lang install` to download them now.";

/// `basemind comms daemon`.
pub const COMMS_DAEMON: &str = "\
When to use: running the daemon in the foreground, under a service manager or to debug it.

Returns: nothing until shutdown: it binds the machine's singleton socket and serves every client.
Set `BASEMIND_NO_AUTOSPAWN` in the clients so none starts a second one.

Example:
  basemind comms daemon

Next: `basemind comms status` from another shell.";

/// `basemind comms start`.
pub const COMMS_START: &str = "\
When to use: starting the daemon ahead of the first session; a session starts it when needed.

Returns: once the daemon answers; a no-op when one is already alive. The daemon keeps the
environment of the process that started it; this explicit start also passes it the `ALLOW`
grants, which a session's implicit start drops.

Example:
  basemind comms start

Next: `basemind comms status` to see it.";

/// `basemind comms stop`.
pub const COMMS_STOP: &str = "\
When to use: after an upgrade, so the next session starts the new version, or to apply changed
environment variables.

Returns: once the daemon has drained and stopped. `--all` stops every live daemon of this
version's protocol generation on the machine.

Example:
  basemind comms stop

Next: `basemind comms start`, or let the next session start it.";

/// `basemind comms status`.
pub const COMMS_STATUS: &str = "\
When to use: checking which daemon is running and since when.

Returns: its pid, version, build id, protocol generation, uptime and the agent store's state,
and a warning when it runs a different build of this version.

Example:
  basemind comms status

Next: `basemind comms stop` when it runs an older build.";

/// `basemind comms doctor`.
pub const COMMS_DOCTOR: &str = "\
When to use: too many daemons, or a daemon that will not serve.

Returns: every live daemon of this protocol generation (pid, directory, version, uptime), flagging
a pile-up over the ceiling, and prunes dead registry entries. `--probe` also asks each one whether
it can serve; `--clear-fatal` deletes the records of scans that died without unwinding.

Example:
  basemind comms doctor --probe

Next: `basemind comms stop --all` to clear them.";

/// `basemind comms cleanup`.
pub const COMMS_CLEANUP: &str = "\
When to use: reclaiming the rendezvous directories of other protocol generations and older
releases.

Returns: the directories removed, with sizes: those no daemon uses, unlocked and idle for
`--older-than` days (default 30). `--dry-run` lists them and removes nothing.

Example:
  basemind comms cleanup --dry-run

Next: `basemind comms doctor` for the daemons still alive.";

/// `basemind agent post`.
pub const AGENT_POST: &str = "\
When to use: telling the other agents of this repository that you start, finish or decide
something, or replying to them.

Returns: the posted message's id and its thread. Pass exactly one of `--subject` (a new thread),
`--thread` or `--reply-to`; `--to` makes a new thread invitation-only and `--paths` scopes it to a
surface.

Example:
  basemind agent post --subject \"starting the parser split\" --body \"src/parse/ is mine.\"

Next: `basemind agent inbox` for the replies.";

/// `basemind agent inbox`.
pub const AGENT_INBOX: &str = "\
When to use: on start and between steps, to read what the other agents wrote to you.

Returns: the oldest unread messages across your threads, bodies inline, marked read unless
`--peek`. `--wait-secs` (at most 40) waits for one; `--id` reads one message whole.

Example:
  basemind agent inbox --peek

Next: `basemind agent post --reply-to <message> --body <text>` to answer.";

/// `basemind agent threads`.
pub const AGENT_THREADS: &str = "\
When to use: finding the threads about a surface before you work on it, or joining, leaving,
inviting to or archiving one.

Returns: the threads you are in or that match `--query` / `--path`, or one thread's details with
`--thread`; `--join`, `--leave`, `--invite` and `--archive` change it.

Example:
  basemind agent threads --path src/auth/

Next: `basemind agent threads --thread <id> --join` to follow one.";

/// `basemind agent status`.
pub const AGENT_STATUS: &str = "\
When to use: checking who this checkout acts as and whether the agent store is up.

Returns: the agent id (from `BASEMIND_AGENT_ID`, `comms.agent_id` or the derived default) and the
state of the daemon's agent store.

Example:
  basemind agent status

Next: `basemind agent inbox` to read your messages.";

/// `basemind agent cleanup`.
pub const AGENT_CLEANUP: &str = "\
When to use: as the operator, archiving threads whoever created them.

Returns: the threads archived by `--archive`, then the result of one retention sweep.

Example:
  basemind agent cleanup --archive t-abcdef

Next: `basemind agent threads --archived` to review them.";

/// `basemind agent hook pre-tool-use`.
pub const AGENT_HOOK_PRE_TOOL_USE: &str = "\
When to use: only from a plugin's `PreToolUse` hook.

Returns: the hook answer that adds the session-scoped `as` to an agent tool call that has none,
read from the event on stdin.

Example:
  basemind agent hook pre-tool-use < event.json

Next: `basemind agent hook name` for the name alone.";

/// `basemind agent hook name`.
pub const AGENT_HOOK_NAME: &str = "\
When to use: only from a plugin hook that needs the session-scoped agent name.

Returns: the name for the event on stdin: the subagent's when the event came from one.

Example:
  basemind agent hook name < event.json

Next: `basemind agent status` for the checkout's agent id.";

/// `basemind workspace workspaces`.
pub const WORKSPACE_WORKSPACES: &str = "\
When to use: listing every workspace the machine's daemon knows, git and plain.

Returns: each registered workspace's key, kind (`git` or `plain`), root and repository id.

Example:
  basemind workspace workspaces

Next: `basemind workspace worktrees <repo-id>` for one repository's worktrees.";

/// `basemind workspace worktrees`.
pub const WORKSPACE_WORKTREES: &str = "\
When to use: listing the worktrees of a registered repository.

Returns: the worktrees of the repository id (the normalised remote URL or `path:<root>`).

Example:
  basemind workspace worktrees <repo-id>

Next: `basemind workspace branches <repo-id>` for its branches.";

/// `basemind workspace branches`.
pub const WORKSPACE_BRANCHES: &str = "\
When to use: listing the local branches of a registered repository.

Returns: the branches of the repository id.

Example:
  basemind workspace branches <repo-id>

Next: `basemind workspace worktrees <repo-id>` for the checkouts.";

/// `basemind daemon ensure`.
pub const DAEMON_ENSURE: &str = "\
When to use: configuring an MCP host that dials streamable HTTP instead of launching
`basemind serve`.

Returns: the daemon's base MCP URL once its HTTP transport is ready, starting the daemon when
none answers. The HTTP front-end binds only when `BASEMIND_ALLOW_HTTP` is set.

Example:
  basemind daemon ensure

Next: give the printed URL to the MCP host.";
