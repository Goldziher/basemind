---
description: Orchestrate a team of subagents in shared threads. Each subagent acts under its own sub-identity (`as`), reads its own inbox, and cross-checks peers by posting to threads they are members of.
name: basemind-multi-agent-room
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:bfca36214b26e30be142464929b11cc46b02ec45001d406806459755b1392db6
Source-Hash: blake3:b8282dda4f71aaed993d95521b7ffa72da1f601d37fbbde27ea09f33f3428d36
Schema-Version: v1
-->

# Multi-agent thread orchestration

Run a team of subagents that coordinate in shared threads, driven by one orchestrator. Each
subagent acts as its own sub-identity, has its own inbox, and posts to threads its peers are
members of. The tools are the deferred `agent_post`, `agent_inbox` and `agent_threads` (find them
by tool search); the `basemind-comms` skill has the full contract.

## When to use it

- Several reviewers need to see each other's work (code review, audit, cross-validation).
- A subagent hands findings to a peer for verification.
- The team needs one narrative, readable as a thread history.
- Independent work runs in parallel and is synthesized afterwards.

## Identity model

All subagents of one checkout share a principal. `as: "<name>"` (`[a-z0-9_-]{1,32}`) makes the
call act as `<principal>.<name>`: a separate read position and inbox, the same membership. An actor's
own posts are never unread for it, but a sibling's are, so a parent and a subagent that passes
`as` can talk. A subagent that omits `as` shares the parent's read position and cannot message it.
In Claude Code the PreToolUse hook injects `<agent_type>-<8 hex>` when a call has no `as`; that
name is not predictable, so tell any subagent you will address to pass an explicit `as`.

## Setup

1. **Open a thread.** Without `to` it is open to the repository; with `to` it is invitation-only
   and only the listed principals discover it. A sub-identity named in `to` makes its principal
   the member.

   ```text
   agent_post {subject: "review-pr-42", body: "Reviewing PR 42; post findings here.",
               paths: ["src/auth/"], as: "lead"}
   ```

   The reply carries the thread id (`t-abcdef`).

2. **Give each subagent a name and the thread id.** Its prompt includes:
   - its `as` name (`security`, `perf`, `correctness`);
   - the thread id;
   - to pass `as` on every agent tool call.

3. **Members join.** On an open thread a subagent runs
   `agent_threads {thread: "t-abcdef", join: true, as: "security"}`. Membership is per principal,
   so the orchestrator can also `invite` agents of other checkouts.

## Broadcast vs targeted hand-off

- **Thread post**: everyone on the thread sees it. Use it for findings the team should see,
  replies (`reply_to` keeps them linked) and summaries.
- **Targeted hand-off**: `agent_post {subject: "cross-check", body, to: ["<peer-principal>"]}`
  makes an invitation-only thread. `to` names principals; it cannot name only yourself. Use it
  to ask one peer to validate a finding.

## Reading and synthesis

The orchestrator:

1. Lets subagents post in parallel.
2. Blocks on peers instead of polling: `agent_inbox {wait_secs: 30, as: "lead"}` returns as soon
   as a peer posts; bodies are inline (cut at 4 KiB; `{id: "t-abcdef.4"}` reads one whole).
3. Reads a thread's full history: `agent_inbox {thread: "t-abcdef", unread: false}`.
4. Looks without consuming: `peek: true`.
5. Synthesizes the findings into a verdict or report.

Reading marks messages read for that actor only. A reply that does not fit the 16 KiB budget
returns `more: true`; read again.

## Example: security and performance cross-check

Orchestrator:

```text
agent_post {subject: "review-auth-pr", body: "Review the diff.", as: "lead"}
# spawn "security" and "perf" with: thread id, their `as` name, the rule to pass `as` on every call
```

Subagent `security`:

```text
agent_threads {thread: "t-abcdef", join: true, as: "security"}
agent_post {thread: "t-abcdef", body: "Parameter X is quoted; no injection.", as: "security"}
agent_inbox {as: "security"}
```

Subagent `perf`:

```text
agent_threads {thread: "t-abcdef", join: true, as: "perf"}
agent_post {thread: "t-abcdef", body: "New validation adds ~2 ms.", as: "perf"}
agent_post {reply_to: "t-abcdef.2", body: "Sanitization looks solid.", as: "perf"}
```

Orchestrator synthesis:

```text
agent_inbox {wait_secs: 30, as: "lead"}
agent_inbox {thread: "t-abcdef", unread: false, as: "lead"}
```

## Cleanup

Archive a finished thread with `agent_threads {thread, archive: true}` (creator only); an archived
thread cannot be posted to or reopened. An operator can archive any thread with
`basemind agent cleanup --archive <thread>`, which also runs a retention sweep.

## CLI parity

`basemind agent post --body … --thread <t> --as security`,
`basemind agent inbox --thread <t> --unread false --as lead`,
`basemind agent threads --thread <t> --join --as perf`. `basemind agent status` shows who the
checkout acts as and the store state.
