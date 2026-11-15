---
status: accepted
date: 2026-11-15
amends: ["0013"]
---

# ADR-0021: Agent comms v2 — stable identity, per-actor delivery, three deferred tools

- **Deciders:** basemind maintainers
- **Related:** amends ADR-0013 (caller-supplied idempotency keys); ADR-0014, ADR-0018, ADR-0020,
  ADR-0024, ADR-0025; normative detail in [`../comms-v2.md`](../comms-v2.md)

## Context

Agents working in one repository exchange messages through a daemon shared by every session of one
operating-system user. ADR-0020 left that surface as the mode-dispatched `agents` tool (18 modes in
`src/mcp/mode.rs`) until its model was settled. An audit of the code on `main` found:

- **Identity is not stable.** `src/comms/identity.rs` has two resolvers. `resolve_session`, used by
  `basemind serve`, generates a new `session` id for every MCP connection unless `BASEMIND_AGENT_ID`
  or the `comms.agent_id` config key is set. `resolve`, used by the CLI and so by the plugin hooks,
  returns an id persisted in the workspace cache. A reconnect is a new agent, and the hooks read an
  inbox the MCP session is never addressed in.
- **Any id can be claimed.** `as_agent` accepts any well-formed id (`resolve_comms_client` in
  `src/mcp/helpers_comms.rs`), and `on_post` in `src/comms/daemon_handlers.rs` checks membership
  only for a `reply_to` target.
- **History is disposable.** `COMMS_SCHEMA_VER` equals the release minor (`src/comms/mod.rs`) and an
  older store is deleted on open (`src/comms/store.rs`), so every minor release erases all threads.
  Messages live three days (`MESSAGE_TTL`).
- **The tools are the only part in use.** ADR-0018 measured that all 650 basemind MCP calls in about
  1,450 transcripts were agent messaging.

## Decision

The rules below are the decision. Field lists, limits, cursor and replay rules, the store layout and
error shapes are in `docs/comms-v2.md`, which changes without a new ADR as long as it stays inside
these rules.

1. **One identity resolver** for `serve`, the CLI and the hooks: `BASEMIND_AGENT_ID` verbatim, else
   `comms.agent_id`, else the derived id `<host>-<repo>-<6 hex of blake3(canonical absolute root)>`.
   "Host" is the agent harness (Claude Code, Codex and so on), read from `BASEMIND_HOST`, which the
   plugin launchers set. With `BASEMIND_HOST` unset the host part is `agent`, so every such harness
   in one checkout shares one identity. The resolved id is the **principal**. A call may pass
   `as: "<name>"` (`[a-z0-9_-]{1,32}`) to act as the sub-identity `<principal>.<name>`; `.` is
   reserved as that separator. An **actor** is a principal or one of its sub-identities.
2. **The principal is fixed per connection** by the client's `Hello`, not per request. The daemon
   derives the caller (principal, sub-identity, repository) from the connection and the call
   envelope; no tool field names the sender.
3. **This prevents accidents, not impersonation.** Any process of the same operating-system user can
   connect and send any `Hello`. Invitation-only threads, repository scoping and creator-only
   archive are conventions between cooperating agents and give no confidentiality. There is no
   per-agent token.
4. **Membership belongs to the principal; the read position belongs to the actor.** A sub-identity
   reads every thread its principal is a member of, with its own read position, which starts at the
   thread's head when the actor is first seen. In Claude Code the plugin hooks inject a `as` scoped
   to the session: derived from `agent_type` and `agent_id` for a subagent and from the session id
   for the main session. Two sessions of one harness in one checkout therefore do not consume each
   other's mail.
5. **Reading marks read.** `agent_inbox` advances the actor's read position past what it returns;
   `peek: true` reads without advancing. There is no `ack`.
6. **Three tools replace `agents`:** `agent_post`, `agent_inbox`, `agent_threads`. They stay
   deferred (not marked always-load) under their own 5,000-byte schema ceiling. The store operation
   behind `agent_inbox` never blocks; the daemon owns the wait (`wait_secs`, 0 to 40).
7. **Retention is the sweep's job alone.** The daemon's periodic sweep deletes messages older than
   30 days by the wall clock at sweep time, keeps at most 1,000 messages per thread, and removes
   threads whose last activity is older than 30 days. A read never hides a record because of its
   age. A timestamp is `max(clock, previous + 1)` and is used for display and by the sweep only;
   order comes from a per-thread `seq` and a store-wide post counter, never the clock.
8. **Caps.** A repository holds at most 256 active threads; creating one at the cap archives the
   least recently active thread instead of refusing. A thread has at most 256 members. A message
   body is at most 16 KiB; a read returns at most 4 KB of a body within a 16 KB response budget, and
   the whole body is read by message id. `basemind agent cleanup` (CLI) may archive any thread.
9. **Storage stays on Fjall**, in its own directory `agents.fjall`, with `COMMS_STORE_VER` decoupled
   from the release minor. Its starting value, 1000, is arbitrary and above every legacy value (the
   legacy version was the release minor, 33 today). Layout changes are forward-only migrations,
   never a wipe. A store newer than the binary opens for reading and refuses writes. A store that
   cannot be opened is left untouched, and `basemind agent status` reports it with the recovery
   step: rename or remove the `agents.fjall` directory. The legacy `store.fjall` is not imported: it
   is renamed once to `store.legacy-<date>`, and the sweep deletes it after 30 days.
10. **A post is committed with Fjall's buffered persist**, not synced to disk per post.
11. **Wire.** The comms protocol version goes from 5 to 6 and the relay protocol from 1 to 2. The 18
    `agents` modes are removed. The `workspace` tool and worktree claims were already removed in
    commit `f976d669` without a protocol bump: a retired request fails to decode and the daemon
    drops that one link, whereas a bump would have refused every older client at `Hello`.
12. **Amendment of ADR-0013.** Its decision "`agents` `post` takes an `idempotency_key`" no longer
    holds: no tool has such a field. Replay protection is keyed only on the random key the stdio
    relay generates for each request and re-sends on a replay. Its "protocol v4" is v6, and its
    40-second cap on the `wait` mode is the upper bound of `wait_secs`. The rest of ADR-0013 stands.
13. **Not over HTTP.** The HTTP front-end (`src/comms/http_frontend.rs`) takes `?agent=` on every
    request, which contradicts rule 2, so the agent tools are not exposed there.
14. **CLI parity (ADR-0014).** `basemind agent` mirrors the three tools. One exception: a CLI call
    is one process making one call with no relay, so a CLI read that marks has no replay protection.

### Constraints left for the agent-launch ADR

- A launched agent gets `BASEMIND_AGENT_ID` and `BASEMIND_HOST` from its launcher.
- The id grammar expresses no parent/child relation between agents.
- Presence is a count of live sessions per principal, not an addressable session.
- A sandboxed harness may see a different canonical root and so derive a different id.
- Adding a token later needs another protocol bump.

## Consequences

- Breaking change to the MCP tools, the CLI, the wire protocol and the store, taken in one release
  with the hooks, skills and docs (ADR-0026). Existing threads and messages are not carried over.
- **Deferral.** The tools account for all measured MCP use, yet stay deferred: those calls were made
  by agents whose instructions told them to coordinate, and the always-loaded byte budget
  (ADR-0020) is spent on code navigation, which no agent was using. Discovery depends on the
  session-start hook and the server instructions naming the three tools; a harness with neither
  finds them only by tool search.
- **Hosts without hooks share one read position.** Outside Claude Code nothing injects `as`, so for
  those hosts an identity is a work queue: the first reader takes the message.
- **Mark-on-read is at-most-once.** A reply lost after the position advanced leaves those messages
  read and never shown; an explicit `ack` would have re-delivered them. `agent_inbox` with
  `unread: false` reads them again from history.
- **Mail sent before an actor exists is not unread for it.** A session-scoped actor starts at each
  thread's head, so messages posted while no session was running are history, not unread.
- **A wrong clock:** ordering is unaffected; a clock ahead at sweep time deletes old messages early
  and permanently; a stamp written ahead of the clock delays that record's deletion until the clock
  passes it.
- **Power loss** can lose the last posts, and their `seq` values are then issued again, so a message
  id a client still holds can name a different message. A daemon crash loses nothing.
- At the thread cap a post can archive a thread its members still wanted; it stays readable.
- Two clones or worktrees of one repository share a repository id, and therefore threads and the
  256-thread cap.
- The legacy store is unreadable by the new binary from the first start and gone after 30 days.

## Alternatives considered

- **SQLite for the store** — rejected: no SQLite crate is linked today (`Cargo.lock` has none), it
  adds a C build to every release target, and Fjall with its own version constant meets the need.
- **Ack-based delivery** — rejected: a second call per read, and an agent that skips it sees the
  same messages again on every read. The price is the at-most-once delivery above.
- **Per-session identities by default** — rejected: that is today's churn. An MCP server is not told
  which host session is calling, so such an id cannot be stable across reconnects or shared with the
  hooks. Sessions are separated by the hook-injected `as` instead, under one stable principal.
- **Importing the legacy store** — rejected: its agent ids are mostly per-connection, its messages
  older than three days are already deleted, and its threads carry no repository.
- **Read-time expiry** (hide a record once the clock says it is too old) — built, then removed:
  three review rounds went to wrong-clock cases, and it needed a clock-tolerance rule that the
  sweep-only model does not.
- **Syncing each post to disk** — rejected: one fsync per post, taken under the daemon's single
  write gate (ADR-0013), so every poster waits on the disk to protect against power loss only.
- **A per-agent token now** — rejected: every process of the same user can read the secret, so it
  adds no boundary; it is left to the agent-launch ADR.
