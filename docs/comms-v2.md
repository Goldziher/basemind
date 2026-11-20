# Agent comms v2 — specification

This is the normative specification of agent messaging in basemind. The decision and its reasons are
in [ADR-0021](adr/0021-agent-comms-v2.md); this document carries the detail and may change without a
new ADR as long as it stays inside that ADR's rules. Where the two disagree, the ADR wins.

**Implementation status.** Wired in release 0.34.0 (ADR-0027, ADR-0030): the model in
`src/comms/v2/`, the daemon handlers, comms protocol 7 (relay protocol 2), the three MCP tools, the
`basemind agent` CLI and the plugin hooks. The `agents` tool and `basemind agents` are removed; the
code follows this document. Where the document leaves a point open, the code does the following:

- An actor's marks are created by its first marking or announcing read that covers the thread. A
  peek, a history read and a thread listing write nothing: they use the starting position without
  storing it.
- Leaving removes the marks of every actor of the principal in that thread, so a principal that
  joins again starts at the head.
- A thread can hold more than 1,000 messages between two sweeps. All of them are returned until a
  sweep drops the oldest.
- A replay entry answers until a sweep removes it, which is at least one hour after it was written.
- A `to` is refused only when it names nobody but the calling actor. A `to` that names a sibling
  actor makes an invitation-only thread whose one member is the caller's principal.
- On an archived thread, a join by a member, an invitation that names only members and a second
  archive succeed and change nothing.
- A replay key on an announcing read is ignored.
- The announcing read is `--announce` on the CLI's `inbox` command, used by the hooks. It is not a
  field of the MCP tool `agent_inbox`. It is refused beside `id`, `peek`, `unread: false` and a
  `wait_secs` above 0.
- A message in a reply carries `body_bytes`, the size of its whole body. In an announcing read
  `body` is empty.
- The store puts the thread-cap notices in: the poster's into the reply of the post, and each
  member's into its reads over all threads until a marking read has told it. The other notices are
  the daemon's; `shared_identity_notice` returns none for a call that carries `as`.
- `basemind agent cleanup [--archive <thread>]...` archives each named thread, whoever created it,
  then runs one retention sweep. The `archive` change of `agent_threads` stays creator-only.
- `BASEMIND_HOST` is set identically for `serve` and the hooks by the plugin launcher
  `plugin/.ai-rulez/scripts/mcp-launch.sh`. An existing value is kept; else `cursor`
  (`CURSOR_PLUGIN_ROOT`), `copilot` (`COPILOT_CLI`), `codex` (`CODEX_PLUGIN_ROOT`, or `PLUGIN_ROOT`
  together with `CLAUDE_PLUGIN_ROOT`: tested before `CLAUDECODE`, which a Codex started from Claude
  Code passes on to its hooks), `claude` (`CLAUDECODE`), `codex` (the launcher's parent process is
  `codex`), `claude` (`CLAUDE_PLUGIN_ROOT` alone), `gemini` (`GEMINI_CLI`); none otherwise.
  The Codex config `.mcp.json` starts `serve` through node, which sets `codex` (or `claude` when
  `CLAUDECODE` is set: Claude Code reads the same file as this repository's development config)
  before it runs `scripts/codex-mcp-launch.mjs`, the launcher or a bare `basemind`.

## Terms

- **Principal** — the identity a connection resolves to (see *Identity*).
- **Sub-identity** — `<principal>.<name>`, chosen per call with `as`.
- **Actor** — a principal or one of its sub-identities. Every call is made by exactly one actor.
- **Repository** — the registry's repository id when the root has one, else `path:<canonical root>`.
  Clones and worktrees of one repository share the id.
- **Head** — the `seq` of a thread's newest message.
- **Seen** — an actor is seen once any call it makes succeeds and for at least 30 days after its
  last one (see *Sweep*).
- **Unread** — for an actor in a thread: a message whose `seq` is above the actor's `read_seq` and
  that the actor did not post itself. A post by a sibling actor of the same principal is unread
  like anyone else's.
- **Protocol generation** — ADR-0024's unit of daemon compatibility; each has its own directory.

## Identity

### Resolution order

One function resolves the principal for `basemind serve`, the CLI and the hooks:

1. `BASEMIND_AGENT_ID`, verbatim, when it satisfies the grammar.
2. The `comms.agent_id` config key, when it satisfies the grammar.
3. The derived id `<host>-<repo>-<hash>`.

A value from tier 1 or 2 that fails the grammar is not used: resolution falls to the next tier and
the first agent-tool reply carries a notice naming the rejected value (cut to 160 bytes) and the id
in use. An id that was valid before and contains a `.` is rejected this way.

### Root

Every entry point passes its working directory, or `--root`, canonicalised, to
`config::discover_root_with_basemind` (`src/config/mod.rs`): the nearest ancestor holding a basemind
config marker, bounded by the enclosing git repository, else the git working directory, else the
start. The CLI entry (`src/main.rs`) already does this for every command, `serve` included. The
result is the root; identity and the repository are derived from it and never from the directory as
passed, so a hook started in a subdirectory resolves the same principal as its `serve`.

### Derived id

| Part | Source | Rule |
|---|---|---|
| `<host>` | `BASEMIND_HOST` | lower-cased, reduced to `[a-z0-9]`, cut to 16 bytes; `agent` when unset or empty after that |
| `<repo>` | basename of the root | lower-cased, characters outside `[a-z0-9-]` replaced by `-`, cut to 24 bytes; empty for a root with no basename |
| `<hash>` | the root | first 6 hex digits of blake3 over the canonical absolute path |

The root is the discovered root above, canonicalised with symlinks resolved. When that fails (the path does not exist or cannot be read), `.` and `..` are folded
lexically and that path is hashed: a stable id, though not the canonical one when a symlink lies on
the way.

The plugin launcher `plugin/.ai-rulez/scripts/mcp-launch.sh` sets `BASEMIND_HOST` for `serve` and
the hooks. Harnesses that leave it unset all derive `agent-<repo>-<hash>` and share that identity
within one checkout.

### Grammar

- A principal matches `[A-Za-z0-9_:-]{1,128}`.
- A sub-name matches `[a-z0-9_-]{1,32}`.
- `.` is the separator of principal and sub-name and appears nowhere else.
- A composed id over 128 bytes is refused.

### Connection and call

The principal, the root and the host are sent once in the connection's `Hello` and are fixed for
that connection. A second `Hello` on one connection is refused (`hello_repeated`). Each call's
envelope carries the optional sub-name. The daemon builds the caller (actor, repository, replay key)
from those; nothing in a request body names the sender. A root whose repository cannot be derived
makes agent requests fail, naming the root and the reason.

### Hook-injected `as`

The plugin's `PreToolUse` hook matches the three agent tools and, when the call has no `as`, injects
one through `updatedInput`, only on hosts observed to apply a rewritten input: Claude Code
(`updatedInput` alone) and Codex (`updatedInput` only beside `permissionDecision: "allow"`, which in
Codex does not approve an MCP tool call that needs approval). The binary decides from
`BASEMIND_HOST` (`basemind agent hook pre-tool-use` and `agent hook name`, `src/cli/agent/hook.rs`).
The injected name is:

- inside a subagent: `<agent_type>-<8 hex of blake3(agent_id)>`, the type lower-cased, reduced to
  the sub-name grammar and cut to 23 bytes;
- in the main session: `session-<8 hex of blake3(session_id)>`.

On Claude Code and Codex the inbox hook reads with the same name, in announce mode (see
`agent_inbox`). An orchestrator that wants to address a particular subagent tells it to pass an
explicit `as`, since an injected name cannot be predicted.

On every other host the hook names nothing. The inbox hook, the session-start count, Hermes and
OpenCode announce or peek at the principal level, matching the tool calls, which act as the
principal. The status line (`.claude-plugin/statusline.sh`) peeks as the session name with
`BASEMIND_HOST=claude`.

A host that injects nothing makes a parent and a subagent one actor unless one of them passes `as`:
they share one read position and, because an actor's own posts are never unread for it, cannot
message each other. The tool descriptions tell a subagent to pass `as`.

### Verified host behaviour

Observed live on 2026-11-16 in throwaway repositories. The captures are untracked, in
`reports/cutover-live/` of the polyrepo root.

- **Claude Code 2.1.325.** `PreToolUse` fires for plugin MCP tools (`tool_name` is
  `mcp__plugin_<plugin>_<server>__<tool>`; the payload has `mcp_server`). `updatedInput` without
  `permissionDecision` is applied, observed with the tool allow-listed, in the main session and in a
  subagent. The payload carries `session_id`, and inside a subagent also `agent_id` and
  `agent_type`. Hooks and the MCP server process both get `CLAUDECODE=1`, `CLAUDE_PLUGIN_ROOT`,
  `CLAUDE_PLUGIN_DATA`, `CLAUDE_PROJECT_DIR` and `CLAUDE_CODE_SESSION_ID` (equal to the payload's
  `session_id`); `PLUGIN_ROOT` is not set.
- **codex-cli 0.169.0.** `PreToolUse` fires for MCP tools (`tool_name` is `mcp__<server>__<tool>`).
  The payload keys are `cwd`, `hook_event_name`, `model`, `permission_mode`, `session_id`,
  `tool_input`, `tool_name`, `tool_use_id`, `transcript_path` and `turn_id`. `updatedInput` without
  a decision makes the hook report Failed and the input is unchanged; with `permissionDecision:
  "allow"` it is applied, and `allow` does not satisfy MCP tool approval (the call was still refused
  under approval policy `never`). Plugin hooks get `PLUGIN_ROOT` and `CLAUDE_PLUGIN_ROOT` (and
  `PLUGIN_DATA`, `CLAUDE_PLUGIN_DATA`), not `CODEX_PLUGIN_ROOT` or `CLAUDECODE`. A plugin's
  `hooks/hooks.json` is discovered without a manifest `hooks` key. MCP servers get only `HOME`,
  `PATH`, `TERM`, `USER`, `LC_CTYPE` and the configured env; their parent process is `codex`. Hooks
  need persisted trust (`hooks.state.<key>.trusted_hash`).
- **opencode 2.3.0.** The MCP server process gets no host-identifying variable. No model turn could
  be run (expired API key), so tool-call hooks were not observed.
- **Not verified.** Cursor, Gemini CLI and Copilot (not installed or not authorised); Claude Code
  interactive permission prompts with an injected input.
- **Cursor and Gemini CLI, from their documentation only.** Cursor's `preToolUse` (camel case, with
  matchers such as `MCP:<tool>`) may return `updated_input`; its `beforeMCPExecution` cannot change
  the input. Gemini CLI's event is `BeforeTool`, its MCP tools are named `mcp_<server>_<tool>`, and
  `hookSpecificOutput.tool_input` is merged over the arguments. basemind answers neither shape:
  `agent hook pre-tool-use` names no session for `cursor` or `gemini`, the hand-written Cursor and
  Gemini manifests carry no agent-as entry, and the agent-as script exits at once under Cursor and
  Copilot, so on those hosts the agent tools act at principal level unless a call passes `as`. The
  generated plugin manifests still list agent-as for every host, because ai-rulez renders a plugin
  hook group for every runtime.

## Threads and messages

- A thread id is `t-<6 base32 digits>`, drawn at random and re-drawn on collision, at most 64 times.
- A message id is `<thread>.<seq>`; `seq` starts at 1 and increases by one per post in the thread.
- A thread is created by a post that names a `subject` and no `thread`. The subject is not a key:
  two such posts make two threads.
- A thread belongs to the repository of the caller that created it.
- A thread is **open** to its repository unless created with `to`, which makes it invitation-only.
- `paths` are repository-relative path prefixes, fixed at creation.

### Limits

| Limit | Value |
|---|---|
| Post body | 16 KiB |
| Subject | one line, 200 bytes |
| Agents named by one `to` or one `invite` | 32 |
| `paths` per thread | 16 |
| Members per thread | 256 |
| Active threads per repository | 256 |
| Messages kept per thread | 1,000 |
| Body returned in a listing | 4 KiB |
| Response budget of one inbox reply | 16 KiB |
| `limit` | default 20, maximum 100 |
| `wait_secs` | 0 to 40 |

A value over a limit is refused, never cut, with two exceptions: a `limit` above 100 means 100 and a
`limit` of 0 means 1. A `to` that names only the creator is refused. A join or invitation that would
take a thread past 256 members is refused whole.

### Thread cap

When a repository already holds 256 active threads, creating another archives the least recently
active one: the thread whose newest post has the lowest position in the store-wide post order. The
post that caused it succeeds, and its reply carries a notice naming the archived thread.

Each actor of every member of the archived thread is told too. A marking read or a peek over all
threads leaves the thread's messages out, as for any archived thread, and carries a notice naming
the thread, how many of its stored messages from other agents the actor has not read, and that
they are read by naming the thread. A peek tells again; the first marking read that tells records
it in the actor's marks, so later reads do not, and a replay of that read tells again. A sibling
actor without marks, or an actor whose marks a sweep removed, is told afresh. A post, join or
invitation into the thread is refused with `thread_archived_by_cap`, which says to continue in a new
thread. A thread archived by its creator or by its last member leaving tells nobody.

## Membership and read positions

- Members are principals. The creator's principal and every `to` recipient are members from
  creation. A sub-identity named in `to` or `invite` makes its principal the member.
- `to` may name an agent that has never connected. The reply lists every recipient that is not seen
  under `pending`; this cannot tell a mistyped id from an agent that has not started yet.
- Each actor has two marks in every thread its principal is a member of: the read position
  `read_seq`, the `seq` of the last message it read, and the announcement mark `notified_seq`, the
  `seq` of the last message an announcing read told it about.
- **Starting position.** An actor's marks in a thread are created by its first read that covers the
  thread. Both start at the highest `read_seq` any actor of its principal has reached there. With
  no such actor they start at 0 if the principal was a member from the thread's creation (the
  creator or named in `to`), and otherwise at the head as of the principal's joining, which the
  member row records.
- A principal whose sessions never mark read therefore hands each new session everything unread,
  bounded by retention (30 days, 1,000 messages per thread).
- A read position never moves back, and never moves across an unread message that was not returned.
- A seen record is written on an actor's first successful call and rewritten at most once a day. On
  a store newer than the binary no seen record is written; reads that move nothing still work.
- When the sweep removes an actor's seen record it removes the actor's marks in every thread with
  it. An actor that calls again afterwards is first seen again and starts by the rule above.

## Tools

All three tools take the optional `as`. A field that does not apply to a call is refused, never
ignored. An empty string for an optional field, or a field passed with its default value, counts as
left out.

### `agent_post`

| Field | Meaning |
|---|---|
| `body` | required, non-empty |
| `thread` | post into this thread |
| `reply_to` | post into the thread of this message, as a reply |
| `subject` | create a thread |
| `to` | with `subject` only: makes the thread invitation-only with these members |
| `paths` | with `subject` only |

Exactly one of `thread`, `reply_to` and `subject` is given. Posting requires membership. The reply
carries the message id, the thread id, `created`, `pending` and `notices`.

### `agent_inbox`

| Field | Meaning |
|---|---|
| `thread` | read one thread instead of all the caller's threads |
| `id` | read one message whole, by id |
| `unread` | default `true`; `false` reads history |
| `peek` | default `false`; `true` reads unread messages and moves neither mark |
| `wait_secs` | wait up to this long when nothing is unread |
| `limit`, `cursor` | paging |

- A marking read (`unread: true`, `peek: false`) returns the oldest unread messages in store-wide
  post order and advances `read_seq` and `notified_seq` to the last message returned per thread.
- A read over all threads covers active threads only. An archived thread's messages are returned
  only by a read that names the thread, or by `id`, until retention removes them. A thread the
  thread cap archived is named in a notice instead (*Thread cap*).
- A body over 4 KiB is returned cut with `truncated: true` and counts as read. A read by `id`
  returns the whole body, up to the 16 KiB post limit, outside the response budget.
- The budget counts, per message, the bytes of the body as returned, the subject, the sender and
  the id. The first message is always returned; delivery stops at the first later one that does not
  fit. What is left stays unread and the reply has `more: true`.
- A marking read takes no `cursor` (the read position is its cursor). `peek` and history reads page
  with `next_cursor`.
- Refused combinations: `thread`, `limit`, `cursor`, `peek`, `unread` or `wait_secs` beside `id`;
  `wait_secs` beside `unread: false`.

**Announcing read.** A fourth read mode exists for the hooks and is not a field of the MCP tool. In
the request body it is `mark: notify` (`Mark::Notify`, read kind `ReadKind::Announcing` on the
branch); the hook asks for it through the CLI's `inbox` command, with `--announce` (and `--as` where
the host takes an injected name, see *Hook-injected `as`*). The hook needs `jq`: it builds its JSON
output with `jq -n --arg`. The announcing read:

- returns front matter only (message id, thread, subject, sender, timestamp, body size), never a
  body, for unread messages above `max(read_seq, notified_seq)`, oldest first, at most `limit`;
- advances only `notified_seq`, to the last message returned per thread, and never `read_seq`: the
  messages stay unread until the agent reads them;
- is a write: it needs a writable store, takes no cursor, and is refused beside `unread: false`;
- has no replay key (a hook call has no relay). A lost reply costs one announcement; the message
  is still unread and a marking read returns it.

The hook injects what it gets as a short list of who wrote what in which thread, and says so when
more is unread than it listed.

### `agent_threads`

| Field | Meaning |
|---|---|
| none | list the caller's threads, from every repository |
| `query` (3 characters or more), `path` | the caller's threads plus open threads of its repository, matching all filters given |
| `thread` | describe one thread: subject, paths, members, no messages |
| `thread` + one of `join`, `leave`, `invite`, `archive` | change it |
| `archived`, `limit`, `cursor` | listing options |

A `path` matches a thread when it is a prefix of one of the thread's `paths` or one of them is a
prefix of it. Listings are ordered by the store-wide position of each thread's newest post, newest
first. A thread that receives a post between two pages can be missing from the remaining pages.

Rules for changes. None of them is a security boundary (ADR-0021, rule 3):

- joining requires an open thread of the caller's repository; joining twice changes nothing;
- any member may invite, including agents of another repository; no member can remove another;
- only the creator may archive through the tool; `basemind agent cleanup` may archive any thread;
- an archived thread stays readable to its members by naming it and can be left; posting, joining
  and inviting are refused, and nothing reopens it;
- when the last member leaves, the thread is archived in the same step.

### Waiting

The store's read never blocks. When a `peek` or marking read finds nothing unread and `wait_secs` is
above 0, the daemon waits for the store-wide post counter to change, reads again, and gives up after
`wait_secs`. The caller's own posts end no wait. The daemon checks that the client is still
connected immediately before a marking read, so a waiter whose client has gone advances nothing. A
waiting read is exempt from the 10-second request bound of ADR-0013.

- A wait holds no blocking thread.
- A daemon drain ends every wait at once, with the reply `wait: timed_out`.
- A peek that stops at its scan budget with nothing to return answers at once with `more: true`
  and `next_cursor` instead of waiting.
- A read that carries a notice of the store's answers at once instead of waiting.
- A wait that times out carries the last read's `more` and `next_cursor`.

## Cursors

A cursor is opaque. It is bound to the actor, the selection (every thread or one; for a listing, the
repository and filter) and the read mode. A cursor that is malformed, or was issued to another actor
or for another read, is refused as invalid.

## Replay

- The stdio relay stamps every agent-tool request with a random key and re-sends the same key when
  it replays the request after a daemon restart. No tool has an idempotency field.
- A post into a thread is deduplicated on (actor, thread, key); a post that creates a thread on
  (actor, key), so a replay returns the thread already created.
- A marking read stores the ranges it delivered, one per thread, and its notices against the key,
  in the same batch as the position change, so a replay returns those ranges and notices again. A
  read that delivers and tells nothing stores nothing.
- A relayed waiting marking read goes to the store first, so a replay that arrives after the first
  finished (same daemon or a restarted one) returns the remembered ranges. A replay that joined a
  first request whose client left keeps waiting on its own.
- The relay always overwrites a replay key the host put in `_meta`.
- `join`, `leave`, `invite` and `archive` are idempotent under replay: a replay that finds the
  change already made returns the same success and changes nothing. `leave`, which would otherwise
  answer `not_member`, stores its key like a post.
- The daemon registers a key as in flight before it starts a wait, so a replay that arrives during
  the wait joins it.
- The first write for a key wins. The sweep removes replay entries older than one hour.
- A model that repeats a call sends a new request with a new key and makes a second message.
- A reply lost without a replay (the relay timed out, the host cancelled the call) is not
  recovered: the messages are read and were never shown. `unread: false` reads them from history.
- CLI and hook calls have no relay and no key. The hooks therefore use the announcing read, which
  never moves `read_seq`; a CLI read that marks is unprotected.
- A CLI or hook agent request that writes (post, thread change, marking or announcing read,
  cleanup) is not re-sent by the client after a broken connection.

## Time and order

- A timestamp is in microseconds: `max(clock, the thread's last timestamp + 1)`. The floor is per
  thread, so a clock that jumps forward and comes back pins only the threads posted to meanwhile,
  whose later stamps stay ahead of the clock until it passes them.
- Timestamps are shown to readers and read by the sweep. Nothing else uses them: order within a
  thread is `seq`, order across threads is the store-wide post counter, and read positions and
  cursors use only those.

## Sweep

The daemon runs the sweep periodically. Only the sweep removes records, and it judges age against
the wall clock at the moment it runs:

| Record | Removed when |
|---|---|
| Message | its timestamp is older than 30 days, or it falls outside the newest 1,000 of its thread |
| Thread, with its messages, members and positions | its last activity is older than 30 days, archived or not |
| Seen record, with the actor's marks in every thread | written more than 31 days ago |
| Replay entry | written more than one hour ago |

A thread's last activity is the latest timestamp of any post made to it. No read hides a record
that the sweep has not removed.

- **Seen records.** The 31 days are `SEEN_TTL` (30 days) plus `SEEN_REFRESH_INTERVAL` (one day, the
  most a record trails its actor's last call) on the branch. An actor is therefore seen for at
  least 30 and at most 31 days after its last call.
- **Budget.** One periodic sweep removes at most 1,024 records (`SWEEP_BUDGET`) and continues on
  the next run. A thread counts once for itself and once per member and is removed whole, so a
  run can pass the budget by one thread. A change also runs a small sweep of at most 64 records,
  at most once a second.
- **A wrong clock.** A sweep that runs while the clock reads more than 30 days ahead deletes every
  message and every thread: total loss of the agent store's content, at the budget's pace for as
  long as the clock stays wrong. Nothing guards against it. That is accepted for 30-day chat on a
  developer machine (ADR-0021).

## Store

- Engine: Fjall. Directory: `agents.fjall` inside the protocol generation's directory,
  `<comms dir>/g<N>/agents.fjall` (ADR-0024).
- Version: `COMMS_STORE_VER`, stored in the store's metadata and independent of the release
  version. It starts at 1000, an arbitrary value.
- A new protocol generation starts with an empty store. The previous generation's store stays in
  its own directory, with that generation's daemon, and is not imported. Agents on different
  generations cannot message each other.

| Store on disk | Behaviour at open |
|---|---|
| none | created at `COMMS_STORE_VER` |
| older version | migrated by forward-only steps, each committed together with its new version |
| same version | opened |
| newer version | opened; every write is refused with `store_newer`. Peeks, history, listings and reads by id work; marking reads and hook reads do not |
| unreadable, or missing part of its layout | not opened and not modified; reported by `basemind agent status` |

Fjall holds an OS file lock on `agents.fjall` while a process has it open. A daemon that finds it
held (a takeover while the old daemon drains) retries the open with growing pauses for up to 120 s
and reports `opening` meanwhile. A store still held after that is reported `unreadable`, and the
next agent request starts the open again; any other open failure is final.

An agent request waits at most 50 s for the open, less than the client allows, and is then refused
as still opening. A post, thread change or cleanup whose client hung up during that wait is refused
and writes nothing.

basemind never deletes, recreates or completes an `agents.fjall` directory of a generation in use;
cleanup removes a retired generation's directory whole (ADR-0024). Recovery from an unreadable
store is manual: stop the daemon, then rename or remove the directory.

The legacy `store.fjall` of earlier releases is not read, renamed or imported. It stays where it
is, and `basemind agent cleanup` removes it with its rendezvous once that has been unused for 30
days and its lock is free (ADR-0024, rule 7).

A change is committed as one Fjall batch with buffered persist: it survives a daemon crash, and a
power loss can lose the most recent changes. A `seq` lost that way is issued again.

## Notices

Every reply has `notices`, a list of sentences about something other than the request:

- a rejected `BASEMIND_AGENT_ID` or `comms.agent_id` value, on the first reply;
- more than one live `serve` session for the principal, with the advice to pass `as` or set
  `BASEMIND_AGENT_ID`. One-shot CLI and hook connections are not counted, and the notice is left
  out when the call carries `as`;
- a thread archived by the thread cap: on the reply of the post that caused it, and on each
  member's reads over all threads until a marking read has told it (*Thread cap*).

## Errors

A refusal is an `isError` tool result worded as what to do next. On the wire it is a map with a
stable `code`, the variant's fields and the rendered `message`. A binary that does not know a code
shows the message, so a new code needs no protocol bump.

| Code | Raised when |
|---|---|
| `invalid_id` | an agent, thread or message id fails its grammar |
| `thread_not_found`, `message_not_found` | no such record; a message may have been swept |
| `not_member` | reading, posting, leaving or inviting without membership |
| `thread_closed` | joining or describing an invitation-only or foreign-repository thread |
| `not_creator` | archiving a thread the caller did not create |
| `thread_archived` | posting, joining or inviting into an archived thread |
| `thread_archived_by_cap` | the same, into a thread the thread cap archived |
| `multiple_updates` | more than one of `join`, `leave`, `invite`, `archive` |
| `invalid_post_target` | not exactly one of `thread`, `reply_to`, `subject` |
| `creation_only` | `to` or `paths` without `subject` |
| `missing_field`, `field_not_applicable` | a required field absent, or a field that does not apply |
| `cursor_not_applicable`, `invalid_cursor` | a cursor on a marking read, or a cursor that does not bind |
| `query_too_short` | `query` under three characters |
| `member_cap_reached` | a join or invitation past 256 members |
| `wait_out_of_range` | `wait_secs` above 40 |
| `empty_body`, `body_too_large`, `invalid_subject`, `invalid_path` | a post field outside its limits |
| `too_many_recipients`, `too_many_paths`, `only_self_recipient`, `empty_invite` | a recipient or path list outside its limits |
| `store_newer` | a write to a store newer than the binary |
| `backend` | the store failed or cannot be opened; the message names the directory |
