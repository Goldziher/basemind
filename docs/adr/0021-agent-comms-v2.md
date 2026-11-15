---
status: accepted
date: 2026-11-15
amends: ["0013", "0020"]
---

# ADR-0021: Agent comms v2 — stable identity, per-actor delivery, three deferred tools

- **Deciders:** basemind maintainers
- **Related:** amends ADR-0013 (idempotency key, protocol version, bounded waits) and ADR-0020
  (decisions 3 and 4); ADR-0014, ADR-0018, ADR-0024, ADR-0025, ADR-0026; normative detail in
  [`../comms-v2.md`](../comms-v2.md)

## Context

Agents working in one repository exchange messages through a daemon shared by every session of one
operating-system user. ADR-0020 left that surface as the mode-dispatched `agents` tool (18 modes in
`src/mcp/mode.rs`) until its model was settled. An audit of the code on `main` found:

- **Identity is not stable.** `src/comms/identity.rs` has two resolvers. `resolve_session`, used by
  `basemind serve`, generates a new id for every MCP connection unless `BASEMIND_AGENT_ID` or the
  `comms.agent_id` config key is set. `resolve`, used by the CLI and so by the plugin hooks, returns
  an id persisted in the workspace cache. A reconnect is a new agent, and the hooks read an inbox
  the MCP session is never addressed in.
- **Any id can be claimed.** `as_agent` accepts any well-formed id (`resolve_comms_client` in
  `src/mcp/helpers_comms.rs`), and `on_post` in `src/comms/daemon_handlers.rs` checks membership
  only for a `reply_to` target.
- **History is disposable.** `COMMS_SCHEMA_VER` equals the release minor (`src/comms/mod.rs`) and an
  older store is deleted on open (`src/comms/store.rs`). Messages live three days (`MESSAGE_TTL`).
- **The tools are the only part in use.** ADR-0018 measured that all 650 basemind MCP calls in about
  1,450 transcripts were agent messaging.

## Decision

The rules below are the decision. Field lists, limits, cursor and replay rules, the store layout and
error shapes are in `docs/comms-v2.md`, which changes without a new ADR while it stays inside them.

1. **One identity resolver** for `serve`, the CLI and the hooks. `BASEMIND_AGENT_ID` is used verbatim
   when it satisfies the id grammar, else `comms.agent_id` under the same test, else the derived id
   `<host>-<repo>-<6 hex of blake3(root)>`. A rejected value falls to the next tier with a notice.
   "Host" is the agent harness, read from `BASEMIND_HOST`, which the plugin launchers and hooks will
   set (nothing sets it on `main`); unset, it is `agent`. The resolved id is the **principal**. A
   call may pass `as: "<name>"` to act as the sub-identity `<principal>.<name>`. An **actor** is a
   principal or one of its sub-identities.
2. **One root.** Every entry point (serve, CLI, hooks) passes its working directory or `--root` to
   the discovery function `serve` already uses to pick the workspace (`discover_root_with_basemind`
   in `src/config/mod.rs`, on the canonicalised start). Identity is derived from the canonical
   absolute root it returns, never from the directory as passed.
3. **The principal is fixed per connection** by the client's `Hello`. The daemon derives the caller
   from the connection and the call envelope; no tool field names the sender.
4. **This prevents accidents, not impersonation.** Any process of the same operating-system user can
   send any `Hello`. Invitation-only threads, repository scoping and creator-only archive are
   conventions between cooperating agents. There is no per-agent token.
5. **Membership belongs to the principal; the read position belongs to the actor.** A new actor
   starts a thread at the highest read position any actor of its principal has reached there. With
   none, it starts at 0 if the principal was a member from the thread's creation (creator or named
   in `to`), and at the thread's head as of joining otherwise.
6. **Own posts.** An actor's own posts are never unread for it. Posts by a sibling actor of the same
   principal are unread like anyone else's, so a parent and a subagent that uses `as` can talk.
7. **Reading marks read.** `agent_inbox` advances the actor's read position past what it returns;
   `peek: true` reads without advancing. There is no `ack`. The hooks read in a third mode,
   **announce**: it returns front matter only, for unread messages the actor has not been told
   about, records that it told, and never marks read. In Claude Code the hooks also inject a
   session-scoped `as`, so two sessions in one checkout do not consume each other's mail.
8. **Three tools replace `agents`:** `agent_post`, `agent_inbox`, `agent_threads`. They stay deferred
   under one schema ceiling of 5,000 bytes for the three together (`agents` alone is bounded at
   8,000 today). The daemon owns the wait (`wait_secs`, 0 to 40); the store never blocks.
9. **Retention is the sweep's job alone.** The sweep deletes messages older than 30 days by the wall
   clock at sweep time, keeps at most 1,000 per thread, and removes threads idle for 30 days. No
   read hides a record for its age. A timestamp is in microseconds, `max(clock, the thread's last
   stamp + 1)`, and serves display and the sweep only; order is a per-thread `seq` and a store-wide
   post counter.
10. **Caps.** 256 active threads per repository; creating one at the cap archives the least recently
    active instead of refusing. 256 members per thread. A body is at most 16 KiB; a listing returns
    at most 4 KiB of a body within a 16 KiB response budget, and the whole body is read by id.
11. **Archived threads** cannot be posted to or reopened. A read over all threads does not return
    their messages; they stay readable by naming the thread until retention removes them.
12. **Storage stays on Fjall**, in `agents.fjall` inside the protocol generation's comms directory
    (ADR-0024), under its own `COMMS_STORE_VER`. Within a protocol generation, layout changes are
    forward-only migrations, never a wipe. A new generation starts with an empty store; the previous
    generation's store stays with that generation's daemon and is not imported. A store newer than
    the binary opens for reading and refuses writes. A store that cannot be opened is left
    untouched and reported. The legacy `store.fjall` of earlier releases is left untouched wherever
    it is; cleanup removes it with its rendezvous once unused for 30 days (ADR-0024, rule 7).
13. **A post is committed with Fjall's buffered persist**, not synced to disk per post.
14. **Wire.** The comms protocol goes from 5 to 6 and the relay protocol from 1 to 2. This is not a
    protocol generation change: within a generation a mismatch fails at `Hello`, and a newer client
    may replace an older idle daemon under ADR-0024's rules.
15. **Amendment of ADR-0013.** No tool has an `idempotency_key`: replay protection is keyed on the
    random key the stdio relay generates per request and re-sends on a replay. "Protocol v4" is v6,
    the 40-second cap on `wait` is the upper bound of `wait_secs`, and a waiting read is exempt
    from the 10-second request bound.
16. **Amendment of ADR-0020.** Decision 3 ends here: `agents` is removed, not kept. Decision 4
    (`src/mcp/mode.rs` is trimmed, not deleted) lapses with it: the file goes with its last mode.
17. **Not over HTTP.** The HTTP front-end takes `?agent=` on every request
    (`src/comms/http_frontend.rs`), which contradicts rule 3.
18. **CLI parity (ADR-0014).** `basemind agent` mirrors the three tools and adds `status` and
    `cleanup`. A CLI call has no relay, so a CLI read that marks has no replay protection.

### Constraints left for the agent-launch ADR

- A launched agent gets `BASEMIND_AGENT_ID` and `BASEMIND_HOST` from its launcher. Whether one whose
  id fails the grammar should fail to start, where rule 1 falls through, is left to that ADR.
- The id grammar expresses no parent/child relation; presence is a count of live sessions per
  principal; a sandboxed harness may see another canonical root; a token needs a protocol bump.

## Consequences

- Breaking change to the MCP tools, the CLI, the wire protocol and the store, taken in release 0.35
  with the hooks, skills and docs (ADR-0026). Existing threads and messages are not carried over.
  The same holds at every later generation change, and agents on different generations cannot
  message each other.
- **Old and new binaries of one generation.** While a daemon of the new protocol runs, an older
  client is refused at `Hello` and has no messaging. After a forward migration, older binaries of
  the generation are read-only: no post, no marking read, and hook reads fail, until upgraded.
- **Deferral.** Discovery depends on the session-start hook and the server instructions naming the
  three tools; a harness with neither finds them only by tool search.
- **Hosts without hooks.** Nothing injects `as`, so parent and subagent are one actor: they share
  one read position and cannot message each other (rule 6). The tool descriptions tell subagents
  to pass `as`.
- **Mark-on-read is at-most-once**, except a relay replay within one hour, which returns the same
  ranges. A reply lost without a replay (relay timeout, host cancel) leaves those messages read
  and unshown; `unread: false` reads them from history.
- **Backlog.** A principal whose sessions never mark read hands each new session everything
  unread, bounded by retention.
- **Actors accumulate:** one per subagent run and per session. An actor's positions are deleted
  with its seen record, 30 to 31 days after its last call (the branch's `SEEN_TTL` plus its
  one-day refresh interval), and a returning actor is first seen again.
- **A wrong clock.** Ordering is unaffected. A forward glitch delays deletion only in threads
  posted to during it. A sweep while the clock reads more than 30 days ahead deletes every message
  and thread — total loss of the agent store's content — and that is accepted for 30-day chat on
  a developer machine.
- **Power loss** can lose the last posts, and their `seq` values are then issued again, so a held
  message id can name a different message. A daemon crash loses nothing.
- **Cap-archive.** A post at the thread cap can archive a thread its members still wanted. Only
  the poster is told; the other members get no notice.
- Two clones or worktrees of one repository share a repository id, and so threads and the cap.

## Alternatives considered

- **SQLite for the store** — rejected: no SQLite crate is linked today (`Cargo.lock` has none), it
  adds a C build to every release target, and Fjall with its own version constant meets the need.
- **Ack-based delivery** — rejected: a second call per read, and an agent that skips it sees the
  same messages on every read. The price is the at-most-once delivery above.
- **Per-session identities by default** — rejected: an MCP server is not told which host session is
  calling, so such an id cannot be stable across reconnects or shared with the hooks.
- **Every new actor starts at the head** — rejected: a worker started after its instructions were
  posted would never see them, and `to` could not address an agent that has not started.
- **Importing the legacy store, or carrying a store across generations** — rejected: legacy ids are
  mostly per-connection and its threads carry no repository; retention bounds the loss to 30 days.
- **Read-time expiry** (hide a record once the clock says it is too old) — rejected: a read would
  then depend on the clock, and a clock briefly behind or ahead would hide or reveal records
  between two reads; it needs a clock-tolerance rule that sweep-only retention does not.
- **Syncing each post to disk** — rejected: one fsync per post under the daemon's single write gate
  (ADR-0013), so every poster waits on the disk to guard against power loss only.
- **A per-agent token now** — rejected: every process of the same user can read the secret.
