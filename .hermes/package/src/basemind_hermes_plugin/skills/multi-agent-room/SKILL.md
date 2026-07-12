---
name: multi-agent-room
description: >-
  Orchestrate a team of named subagents in shared threads. One orchestrator drives
  multiple peers with distinct identities; each subagent sees its own inbox and can
  cross-check findings by posting to a thread the peer is a member of.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:525b3f8031902107da61ff0f2a5e71f24e1e3bc25f8b5a39bcef00c712bc8894
Source-Hash: blake3:ca37d0bd1accfd4a3156158a1f47fd4568c00994d83f95330f41124a857f34dc
Schema-Version: v1
-->

# Multi-agent thread orchestration

Run a team of NAMED subagents that coordinate in shared THREADS, all driven by one
orchestrator. Each subagent has its own identity and inbox, and posts to threads the
relevant peers are members of.

## When to use it

Orchestrate subagents when:

- Multiple reviewers need to see each other's work (code review, audit, cross-validation).
- A subagent should hand off findings to a peer for verification.
- The team needs a shared narrative (all readable in one thread history).
- You want to parallelize independent work (each agent runs concurrently) and then synthesize.

## Setup

1. **Start a thread and name its members.** A thread is addressed by at least two of three
   coordinates — `{subject, path-glob, members}`. For a private team, list the members explicitly
   so only they see it; discovery is scoped, never global, so unrelated agents never surface it.

   ```text
   thread_start {subject: "review-pr-42", members: ["security", "perf"], title: "Code review panel"}
   ```

2. **Assign each subagent a short name.** Pass `as_agent` to every tool call the subagent makes.
   Names like `"security"`, `"perf"`, `"correctness"` are clearer than defaults. A named member
   must `thread_join` (or be added via `thread_add_member`) before it can post.

3. **Distribute the subagent contract.** Each subagent's prompt should include:
   - Its assigned `as_agent` name (e.g. `"security"`).
   - The shared thread's subject (e.g. `"review-pr-42"`).
   - Instructions to:
     - Call `agent_register {as_agent: "security", name: "Security Reviewer", …}` once.
     - Call `thread_join {thread: "review-pr-42", as_agent: "security"}` to participate.
     - Call `thread_post` with `as_agent: "security"` to share findings.
     - Call `inbox_read {as_agent: "security"}` to see cross-thread messages addressed to it.

## Broadcast vs targeted hand-off

- **Thread post** (`thread_post` + `as_agent`): everyone on the thread sees it. Use for:
  - Announcing a finding the whole team should see.
  - Replying to a peer's discovery (`reply_to: <id>` keeps it linked).
  - Summarizing your work.

- **Targeted hand-off**: to reach one peer privately, `thread_start` a two-member thread
  (`members: ["security", "perf"]`) — only those two discover it. Use for:
  - Asking a peer to cross-check your finding.
  - Sharing a detail not ready for the whole team.
  - Hand-off: `"I found X; can you validate my approach?"`.

## Reading and synthesis

As the orchestrator:

1. Let all subagents post to their threads (in parallel).
2. Read the shared thread: `thread_history {thread: "review-pr-42"}` (front-matter only).
3. For each message that matters, fetch the body: `message_get {message_id: "…"}`.
4. Read each subagent's inbox: `inbox_read {as_agent: "security"}`, etc.
   (Messages appear as front-matter; bodies come from `message_get`.)
5. Synthesize: combine the findings into a verdict, decision, or report.

## Example: two-agent security + performance cross-check

**Orchestrator setup:**

```text
thread_start {subject: "review-auth-pr", members: ["security", "perf"]}

# Spawn agent "security"
# Prompt: "You are agent 'security' on thread 'review-auth-pr'. Register yourself,
# join the thread, analyze the diff for auth bugs, post findings, and start a
# two-member thread with 'perf' asking them to check your fix's perf implications."

# Spawn agent "perf"
# Prompt: "You are agent 'perf' on thread 'review-auth-pr'. Register yourself,
# join the thread, analyze the diff for performance regressions, post findings, and
# reply to 'security' with your take on their auth fix."
```

**Agent "security" steps:**

```text
agent_register {
  as_agent: "security", name: "Security Reviewer", description: "Auth auditor"
}
thread_join {thread: "review-auth-pr", as_agent: "security"}
thread_post {
  thread: "review-auth-pr", as_agent: "security", subject: "SQL injection check",
  body: "Parameter X is quoted..."
}
thread_start {
  subject: "cross-check-sanitized-input", members: ["security", "perf"], as_agent: "security"
}
thread_post {
  thread: "cross-check-sanitized-input", as_agent: "security", subject: "Cross-check: sanitized input",
  body: "I added validation at line 42..."
}
inbox_read {as_agent: "security", mark_read: true}  # See perf's response
```

**Agent "perf" steps:**

```text
agent_register {
  as_agent: "perf", name: "Performance Reviewer", description: "Latency auditor"
}
thread_join {thread: "review-auth-pr", as_agent: "perf"}
thread_post {
  thread: "review-auth-pr", as_agent: "perf", subject: "Cache impact check",
  body: "New validation adds ~2ms..."
}
thread_post {
  thread: "cross-check-sanitized-input", as_agent: "perf", subject: "Auth fix validated",
  body: "Sanitization looks solid...", reply_to: "msg-sec-2"
}
```

**Orchestrator synthesis:**

```text
thread_history {thread: "review-auth-pr"}  # See full thread
message_get {message_id: "msg-sec-1"}  # Get security's body
message_get {message_id: "msg-perf-1"}  # Get perf's body
thread_history {thread: "cross-check-sanitized-input"}  # See the two-member exchange
message_get {message_id: "msg-perf-2"}  # Get perf's hand-off body
# Synthesize: "Security + perf sign off. Ready to merge."
```

## Recency and thread freshness

Reads default to RECENT so stale chatter never confuses an agent:

- `thread_history` / `inbox_read` return only the **last 24 hours** of messages by default. Pass
  `since_hours: N` for a wider window, or `since_hours: 0` for the full append-only log. Nothing is
  ever deleted — older history stays reachable explicitly.
- Every front-matter row carries `age_secs` (seconds since the message was posted) so you can gauge
  staleness without converting timestamps yourself.
- `thread_list` flags each thread `stale: true` when it has had no post for over 7 days (or never).
  The CLI renders this as an `ACTIVE` / `STALE` marker per thread. Idle threads auto-archive; skip
  stale ones unless you are intentionally reviewing old context.

## Notes

- A named member must `thread_join` (or be added via `thread_add_member`) before it can post.
- A private two-member thread reaches exactly one peer — only its members discover it.
- Front-matter-only reads (history, inbox) are cheap. Fetch bodies only when needed.
- The CLI offers parity: `basemind comms post --as-agent security …`,
  `basemind comms thread-start <subject> --member perf --as-agent security`,
  `basemind comms history <thread> --since-hours 0` (all history),
  `basemind comms threads` (shows ACTIVE / STALE per thread).
