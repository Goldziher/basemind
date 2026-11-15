# ADR-0021: Agent comms v2 — stable identity, repository-open threads, three verb tools

- **Status:** Accepted
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

**Identity.** One resolver serves `serve`, the CLI and the hooks. `BASEMIND_AGENT_ID` is used
verbatim; otherwise the id is `<host>-<repo>-<4 hex of blake3(root)>`, a pure function of the host
name and the workspace root with no pid, clock or file. `BASEMIND_HOST` is set by
`scripts/mcp-launch.sh` and the Codex launcher. The `session-*` and persisted `agent-id` tiers are
removed. Two sessions of one host in one checkout therefore share an identity; the daemon reports
this in a `notice` that says how to split them (`BASEMIND_AGENT_ID`, or `as`). A subagent passes
`as: "<name>"` and acts as `<principal>.<name>`, so a session can only act as its own
sub-identities.

**Token.** `token = base32(blake3_keyed(secret, id))`, where the secret is a 0600 file in the comms
directory. The daemon verifies it on every `Hello`. A launched agent receives its id and token as
`BASEMIND_AGENT_ID` and `BASEMIND_AGENT_TOKEN`. The token stops one process from claiming another's
id by accident; it is not a boundary against a process of the same user, which can read the secret.

**Threads and messages.** A thread id is `t-<6 base32>`. A message id is `<thread>.<seq>`, so a
reference is a point lookup. A thread is created by the first post, which must carry a `subject`,
and belongs to one repository: the registry `RepoId`, shared by the worktrees of a clone. A thread
is open to that repository unless it is created with `to`, which makes it invitation-only.

**Membership and read position.** One table, `members(thread, agent, read_seq, notified_seq)`, with
a reverse `member_threads` index, replaces `thread_members`, `thread_subs`, `Thread.members` and the
`cursors` keyspace. The creator and the `to` invitees start at sequence 0, an agent that joins
starts at the head, and an invited agent starts at the head as of the invitation. Reading advances
`read_seq`; there is no separate acknowledgement.

**Authorization.** All rules live in `src/comms/authz.rs`:

- reading and posting require membership;
- joining requires an open thread in the caller's repository;
- any member may invite;
- discovery returns the caller's own threads plus open threads of the caller's repository that
  match a `query` of at least three characters or a repo-relative `path`.

**Tools.** Three verb tools under the ADR-0018 contract replace `agents`:

| Tool | Fields |
|---|---|
| `agent_post` | `body`; one of `thread`, `reply_to`, `subject`; `to?`, `paths?`, `as?` |
| `agent_inbox` | `thread?`, `id?`, `unread=true`, `peek=false`, `wait_secs` 0–40, `limit=20`, `cursor?`, `as?` |
| `agent_threads` | `query?`, `path?`, `thread?`, `join?`, `leave?`, `invite?`, `archived?`, `limit`, `cursor?`, `as?` |

`agent_inbox` returns bodies inline, up to 4 KB each within a 16 KB response budget, and marks what
it returns as read unless `peek` is set. `agent_threads` with no arguments lists the caller's
threads and the peers in them.

**Removed.** `register`, `ack`, `remove_member` and MCP `archive`; the `workspace` tool with all
five modes; worktree claims in the registry. `cleanup` and `status` remain as CLI commands only.

**Storage.** The store stays on Fjall. `COMMS_STORE_VER = 1000` replaces `COMMS_SCHEMA_VER` and is
no longer tied to `RELEASE_MINOR`, so a release does not wipe comms. The legacy store is imported
once: the three membership copies are unioned, cursors become `read_seq`, and generated
`session-*` agents are dropped. A store that fails to open is renamed, never deleted. Messages are
kept 30 days (three today) and at most 1000 per thread (unchanged).

**Wire.** `PROTO_VER` goes from 5 to 6: `Hello` gains the token, host and root, `Call` gains `sub`
for the sub-identity, and `Post`, `InboxRead`, `ThreadQuery` and `ThreadUpdate` replace the twenty
coordination requests and the five workspace requests in `src/comms/protocol.rs`. The wait runs in
the daemon inside `InboxRead`. `RELAY_PROTO_VER` goes from 1 to 2.

**Amendments to ADR-0013.** Protocol v4 becomes v6. The 40 s cap on `wait` becomes the upper bound
of `wait_secs`, and an `InboxRead` that waits is exempt from the 10 s request bound. Post
idempotency and relay replay keep their rules and are re-keyed on the new requests.

## Consequences

- Breaking change to the MCP tools, the CLI, the wire protocol and the store layout, taken in one
  release with the hooks, skills and docs. No mode aliases are kept.
- `src/mcp/mode.rs`, `src/mcp/lenient.rs` and `DOMAIN_TOOLS_BYTE_CEILING` are deleted, completing
  ADR-0020. The three tools add about 4.5 KB to a verb surface that ADR-0020 measured at about 18 KB
  of the 20 KB `VERB_TOOLS_BYTE_CEILING`; the cutover must either leave them deferred or re-budget
  that ceiling.
- CLI parity (ADR-0014): the 18 `agents` rows and 5 `workspace` rows in
  `tests/cli_parity/capabilities.rs` are replaced by three injective tool rows, and the
  mode-to-subcommand bijection disappears with `domain_modes()`.
- Individual-tier memory is keyed by agent id (`src/mcp/memory.rs`), so it restarts empty under the
  new ids.
- `agent_inbox` is a mutation. A response lost after the daemon advanced `read_seq` leaves those
  messages read; they stay retrievable with `unread=false`.
- Two sessions sharing an identity share one read position and do not see each other's posts as
  unread.
- Nothing prevents two sessions from editing one worktree; coordination is by message only.
- A thread member cannot be removed by another member; a closed thread grows only by invitation.

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
