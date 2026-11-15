# ADR-0021: Agent comms v2 — stable identity, repository-open threads, three verb tools

- **Status:** Proposed
- **Date:** 2026-11-15
- **Deciders:** basemind maintainers
- **Related:** amends ADR-0013; ADR-0014, ADR-0018, ADR-0020

## Context

ADR-0020 kept `agents` (18 modes) and `workspace` (5 modes) as mode-dispatched tools until the
coordination model was settled. No ADR describes that model, and an audit found it unsound as built:

- **Identity churns.** `src/comms/identity.rs` has two resolvers. `resolve_session`, used by
  `basemind serve`, mints `session-<pid>-<nanos>` for every MCP transport unless
  `BASEMIND_AGENT_ID` or `comms.agent_id` is set. `resolve`, used by the CLI and therefore by
  `hooks/inbox-notify` and `hooks/session-start`, returns the id persisted in the workspace cache
  dir. A reconnect is a new agent, and the hooks read an inbox the MCP session never posts to or is
  addressed in. One machine registered 42 `session-*` agents in about two hours; a single
  maintenance pass pruned 319 abandoned agent ids.
- **Authorization is partial.** In `src/comms/daemon_handlers.rs`, `on_post` checks that the thread
  exists but checks membership only for a `reply_to` target; `on_history` takes no session at all;
  `on_thread_join` admits any agent to any thread on the machine. `as_agent` accepts any valid id
  (`src/mcp/helpers_comms.rs`, `resolve_comms_client`), so a session can act as any other agent.
- **Membership has three copies.** The `thread_members` keyspace, its `thread_subs` mirror and
  `Thread.members` are written separately (`src/comms/store.rs`) and read by different paths:
  `on_thread_list` trusts `Thread.members`, `is_member` trusts `thread_members`.
- **Scope does not scope.** `src/comms/scope.rs` matches a thread's path pattern against the
  caller's working directory, not against the files the thread is about, and threads carry no
  repository. The subject filter in `on_thread_list` is an unanchored substring of any length over
  every thread on the machine.
- **Message lookup scans.** Message ids are `<thread>:<agent>:<micros>:<n>`; resolving one
  (`resolve_ids`, `resolve_message_reference`) iterates the whole `messages_by_thread` keyspace.
- **Worktree claims are never released.** `Registry::claim_worktree` (`src/registry/mod.rs`) stores
  a holder with no expiry and no heartbeat, and the holder is usually a `session-*` id that no
  longer exists. All seven claims found on the audited machine were orphaned, and nothing reads
  them.
- **History is disposable.** `COMMS_SCHEMA_VER` is `RELEASE_MINOR`, so every minor release wipes the
  store, and `MESSAGE_TTL` is three days.

## Decision

**Identity.** One resolver serves `serve`, the CLI and the hooks, with three tiers:

1. `BASEMIND_AGENT_ID`, verbatim.
2. The existing `comms.agent_id` config key.
3. A derived id, `<host>-<repo>-<6 hex of blake3(canonical root)>`: a pure function with no pid,
   clock or file. `<host>` is `BASEMIND_HOST` reduced to `[a-z0-9]`, else `agent`. `<repo>` is the
   root directory's basename, lowercased, with every character outside `[a-z0-9-]` replaced by `-`,
   cut to 24 bytes.

The plugin launcher and the plugin hooks set `BASEMIND_HOST` from one shared script, and a test
asserts that the hook identity equals the `serve` identity. The `session-*`, persisted `agent-id`
and generated `agent-*` tiers are removed, and so is the identity collision ledger (`CLAIMS_DIR`,
`CLAIM_TTL`). Identity is trusted per operating-system user; there is no credential.

**Sub-identities.** A subagent passes `as: "<name>"` and acts as `<principal>.<name>`, so a session
can only act as its own sub-identities. `<name>` matches `[a-z0-9_-]{1,32}`. `.` is reserved as the
separator and rejected in a principal id from the environment or config. A composed id over the
128-byte `MAX_ID_LEN` is refused.

The server cannot tell a subagent from its parent, because both use one MCP connection, so it cannot
require `as`. In Claude Code the plugin's `PreToolUse` hook sees `agent_id` inside a subagent and
injects `as` through `updatedInput` on the three agent tools when the caller did not set it. On
other hosts the tool description is the only guard.

**Shared identity.** Two sessions of one host in one checkout resolve to the same id and are not
split. The daemon tracks live connections per principal; while more than one is live, `agent_inbox`
and `agent_threads` replies carry a `notice` that another session shares the identity and that
`BASEMIND_AGENT_ID` or `as` separates them.

**Threads and messages.** A thread id is `t-<6 base32>`, checked for collision at creation and
regenerated. A message id is `<thread>.<seq>`, so a reference is a point lookup. A thread is created
by a post that names a `subject` and no `thread`; the subject is not a key, so two such posts make
two threads. A thread belongs to one repository: the registry `RepoId` when there is one, else
`path:<canonical root>`. Two clones of one remote share a `RepoId` and therefore share threads, by
design. A thread is open to its repository unless created with `to`, which makes it
invitation-only. Its `paths` are repo-relative path prefixes, fixed at creation.

**Membership and read position.** One table, `members(thread, agent, read_seq, notified_seq)`, with
a reverse `member_threads` index, replaces `thread_members`, `thread_subs`, `Thread.members` and the
`cursors` keyspace. The creator and every `to` recipient are members from creation and start at
sequence 0. `to` may name an agent that has never connected, since ids are deterministic; that agent
finds the thread on its first `agent_inbox`. An agent that joins starts at the head, and an invited
agent at the head as of the invitation.

Reading advances `read_seq` to the last message returned in the reply; there is no separate
acknowledgement. A body over 4 KB is returned cut, with `truncated: true`, and counts as read (the
full body comes from `id`). A message left out by the 16 KB response budget does not. The advance is
stored with the request id and the delivered range, so a request replayed after a daemon restart
returns the same range instead of an empty page. A call cancelled after the advance loses that
page from the unread view; `unread: false` reads it again.

**Authorization.** All rules live in `src/comms/authz.rs`:

- reading and posting require membership;
- joining requires an open thread in the caller's repository;
- any member may invite; no member can be removed by another;
- only the creator may archive;
- discovery returns the caller's own threads plus open threads of the caller's repository that
  match a `query` of at least three characters or a `path`. A `path` matches a thread when it is a
  prefix of one of the thread's `paths`, or one of them is a prefix of it.

**Tools.** Three tools under the ADR-0018 contract replace `agents`:

| Tool | Fields |
|---|---|
| `agent_post` | `body`; one of `thread`, `reply_to`, `subject`; `to?`, `paths?`, `as?` |
| `agent_inbox` | `thread?`, `id?`, `unread=true`, `peek=false`, `wait_secs` 0–40, `limit=20`, `cursor?`, `as?` |
| `agent_threads` | `query?`, `path?`, `thread?`, `join?`, `leave?`, `invite?`, `archive?`, `archived?`, `limit`, `cursor?`, `as?` |

`agent_inbox` returns bodies inline and marks what it returns as read unless `peek` is set.
`agent_threads` with no arguments lists the caller's threads and the peers in them;
`{thread, archive: true}` archives a thread the caller created. The three tools are not
always-loaded: they stay deferred, outside `VERB_TOOLS`, under their own 5,000-byte ceiling with its
own test. The 20,000-byte verb ceiling is unchanged.

`agent_post` has no idempotency field. The relay stamps every `Post` with a key derived from the
JSON-RPC request id, so a transport replay is deduplicated; a model that repeats a call makes a
second message.

**Removed.** `register`, `ack`, `remove_member`; the `workspace` tool; worktree claims and their
release, everywhere. `cleanup` and `status`, and the read-only `workspaces`, `worktrees` and
`branches` listings, remain as CLI commands only.

**Storage.** The store stays on Fjall. `COMMS_STORE_VER = 1000` replaces `COMMS_SCHEMA_VER` and is
no longer tied to `RELEASE_MINOR`, so a release does not wipe comms. Later shape changes are
forward-only numbered migration steps run at open. A store newer than the binary is refused for
writing with a message that names the fix, and is never wiped.

The legacy store is not imported. On first start the legacy `store.fjall` directory is renamed to
`comms.legacy-<date>` inside the comms directory, basemind never deletes it, and a fresh store is
created. An import would carry little: most agent ids in it are per-process, messages older than
three days are already deleted, and its threads carry no repository.

**Retention.** Messages are kept 30 days (`MESSAGE_TTL`, three today) and at most 1000 per thread
(`MAX_MESSAGES_PER_THREAD`, unchanged). A thread with no message for 30 days is archived
(`THREAD_IDLE_TTL`, seven days today). An archived thread is removed when its last message expires;
`THREAD_RETENTION_TTL` (14 days) is deleted.

**Wire.** `PROTO_VER` goes from 5 to 6: `Hello` gains the host and root, `Call` gains `sub` for the
sub-identity, and `Post`, `InboxRead`, `ThreadQuery` and `ThreadUpdate` replace the twenty
coordination requests and the two claim requests in `src/comms/protocol.rs`. The wait runs in the
daemon inside `InboxRead`. `RELAY_PROTO_VER` goes from 1 to 2.

**Amendments to ADR-0013.** Protocol v4 becomes v6. The 40 s cap on `wait` becomes the upper bound
of `wait_secs`, and an `InboxRead` that waits is exempt from the 10 s request bound. `InboxRead` is
replayed after a daemon restart, against the stored delivered range. Post idempotency keeps its
rules; the key is no longer a caller field.

## Consequences

- Breaking change to the MCP tools, the CLI, the wire protocol and the store layout, taken in one
  release with the hooks, skills and docs. No mode aliases are kept, and existing threads and
  messages are not carried over.
- `src/mcp/mode.rs`, `src/mcp/lenient.rs` and `DOMAIN_TOOLS_BYTE_CEILING` are deleted, completing
  ADR-0020. A session pays nothing for the agent tools until a host loads them through tool search.
- CLI parity (ADR-0014): the 18 `agents` rows and 5 `workspace` rows in
  `tests/cli_parity/capabilities.rs` are replaced by three injective tool rows, and the
  mode-to-subcommand bijection disappears with `domain_modes()`. The kept listings are CLI commands
  with no MCP tool, which ADR-0018 allows.
- Individual-tier memory is keyed by agent id (`src/mcp/memory.rs`), so it restarts empty under the
  new ids. Most sessions lose nothing, because their `session-*` id already changed on every start.
  Two sessions that share an identity now share individual-tier memory.
- `agent_inbox` is a mutation. A response lost to a cancelled call leaves those messages read.
- Two sessions sharing an identity share one read position and do not see each other's posts as
  unread.
- Outside Claude Code, a subagent that omits `as` acts as its principal and consumes the
  principal's unread messages.
- Any process of the same user can post as any agent. Cross-harness launch may add a credential in
  its own ADR.
- Nothing prevents two sessions from editing one worktree; coordination is by message only.

## Alternatives considered

- **Move the store to SQLite now** — rejected: `rusqlite` is not in `Cargo.lock`, an earlier
  bundled SQLite pin blocked crates.io publishing, and the model change does not need a new engine.
- **Split identities automatically per session** — rejected: an MCP server is not told which host
  session or subagent is calling, so a split id cannot be stable across reconnects or shared with
  the hooks. That is the churn this ADR removes.
- **Keep an explicit `ack`** — rejected: it costs a second call per read, and agents that skip it
  see the same messages again. `peek` covers the case of looking without consuming.
- **Keep worktree claims with a TTL and heartbeat** — rejected: nothing reads claims, and a
  heartbeat needs a live session identity that hosts do not provide.
- **A per-agent token now** (a keyed hash of the id, verified on `Hello`) — rejected: every
  process of the same user can read the secret, so it adds no boundary.
- **A one-shot import of the legacy store** — rejected for the reasons under Storage; the renamed
  directory keeps the data available.
