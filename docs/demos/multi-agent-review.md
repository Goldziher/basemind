# Demo: Code-review panel orchestration

A walkthrough of orchestrating a CODE-REVIEW PANEL using basemind's multi-agent comms.
Three reviewer subagents analyze a code change in parallel, post findings to a shared room,
cross-check via DM, and the orchestrator synthesizes a verdict.

## Prerequisites

- basemind built with `--features comms`
- A working repo with `basemind serve` running
- A known code change to review (e.g., a PR diff)

## Step 1: Create the review room

The orchestrator creates a private team room scoped to the session (auto-joins reviewers
sharing this session id):

```bash
# MCP tool call
room_create {room: "code-review-panel", scope: "session", title: "Code Review: Parallel Panel"}
```

Expected response:

```json
{
  "room": {
    "room_id": "code-review-panel",
    "title": "Code Review: Parallel Panel",
    "created_at": <timestamp>
  }
}
```

## Step 2: Spawn three reviewer subagents in parallel

Launch three independent subagents, each with a distinct role and `as_agent` name:

### Subagent 1: Security Reviewer

**Prompt snippet:**

```text
You are agent "security" on room "code-review-panel".
Register yourself, analyze the code diff for security issues (injection,
crypto, data exposure), post findings to the room with `as_agent: "security"`,
and DM the "correctness" reviewer asking them to validate your fix recommendations.
Use the multi-agent-room skill as a reference.
```

**Tools the subagent calls:**

```text
# Register
agent_register {
  as_agent: "security",
  name: "Security Reviewer",
  description: "Analyzes authentication, injection, and data exposure risks",
  skills: ["auth", "injection", "crypto"]
}

# Post finding to the room
room_post {
  room: "code-review-panel",
  as_agent: "security",
  subject: "SQL injection risk in user lookup",
  body: "Line 42 concatenates user input into a query.
  Recommendation: use parameterized queries or ORM escaping."
}

# DM a peer for cross-check
dm_send {
  to_agent: "correctness",
  as_agent: "security",
  subject: "Cross-check my SQL fix",
  body: "I flagged a potential injection at line 42.
  Can you verify the parameterized approach is correct?"
}

# Read responses (DMs appear in inbox)
inbox_read {as_agent: "security", mark_read: true}
```

### Subagent 2: Performance Reviewer

**Prompt snippet:**

```text
You are agent "perf" on room "code-review-panel".
Register yourself, analyze the code diff for performance regressions (loops,
allocations, I/O), post findings to the room with `as_agent: "perf"`, and
DM the "security" reviewer asking if their fix introduces latency.
```

**Tools the subagent calls:**

```text
# Register
agent_register {
  as_agent: "perf",
  name: "Performance Reviewer",
  description: "Analyzes latency, memory, and algorithmic complexity",
  skills: ["latency", "memory", "big-o"]
}

# Post finding
room_post {
  room: "code-review-panel",
  as_agent: "perf",
  subject: "N+1 query in user search loop",
  body: "Line 56 queries the database inside a loop over users.
  Impact: 1000 users = 1000 queries. Recommendation: batch fetch."
}

# Cross-check security's fix
dm_send {
  to_agent: "security",
  as_agent: "perf",
  subject: "Re: SQL fix impact on latency",
  body: "Your parameterized approach is good. Adds ~0.1ms per query.
  The batching fix is the real win here."
}

# Read inbox
inbox_read {as_agent: "perf", mark_read: true}
```

### Subagent 3: Correctness Reviewer

**Prompt snippet:**

```text
You are agent "correctness" on room "code-review-panel".
Register yourself, analyze the code for logic errors (off-by-one, null safety,
state consistency), post findings to the room with `as_agent: "correctness"`,
and DM the "security" and "perf" reviewers with integration concerns.
```

**Tools the subagent calls:**

```text
# Register
agent_register {
  as_agent: "correctness",
  name: "Correctness Reviewer",
  description: "Analyzes logic, null safety, and state consistency",
  skills: ["logic", "null-safety", "state"]
}

# Post finding
room_post {
  room: "code-review-panel",
  as_agent: "correctness",
  subject: "Null check missing on user ID",
  body: "Line 38 assumes user_id is never null, but the API allows it.
  Add a guard or default value."
}

# Cross-check security
dm_send {
  to_agent: "security",
  as_agent: "correctness",
  subject: "Re: SQL injection fix + null safety",
  body: "Your parameterized fix is solid. Make sure the null guard goes in
  before the query to avoid downstream crashes."
}

# Read inbox
inbox_read {as_agent: "correctness", mark_read: true}
```

## Step 3: Orchestrator reads the shared thread (front-matter)

While the subagents work in parallel, the orchestrator scans the room:

```bash
room_history {room: "code-review-panel"}
```

Expected response (front-matter only, no bodies):

```json
{
  "total": 3,
  "messages": [
    {
      "id": "msg-sec-1",
      "from": "security",
      "subject": "SQL injection risk in user lookup",
      "ts_micros": <ts1>,
      "body_len": 145,
      "body_sha": "<hash1>"
    },
    {
      "id": "msg-perf-1",
      "from": "perf",
      "subject": "N+1 query in user search loop",
      "ts_micros": <ts2>,
      "body_len": 167,
      "body_sha": "<hash2>"
    },
    {
      "id": "msg-corr-1",
      "from": "correctness",
      "subject": "Null check missing on user ID",
      "ts_micros": <ts3>,
      "body_len": 152,
      "body_sha": "<hash3>"
    }
  ]
}
```

## Step 4: Orchestrator fetches bodies selectively

Only fetch the bodies you need to synthesize:

```bash
message_get {message_id: "msg-sec-1"}
message_get {message_id: "msg-perf-1"}
message_get {message_id: "msg-corr-1"}
```

Response for the first:

```json
{
  "message_id": "msg-sec-1",
  "found": true,
  "body": "Line 42 concatenates user input into a query.
  Recommendation: use parameterized queries or ORM escaping."
}
```

## Step 5: Orchestrator reads each subagent's inbox (DMs)

Fetch the DMs that subagents sent to each other:

```bash
inbox_read {as_agent: "security"}
inbox_read {as_agent: "perf"}
inbox_read {as_agent: "correctness"}
```

Each returns front-matter (DMs appear as messages in private pairwise rooms).
Fetch bodies for the critical DMs:

```bash
message_get {message_id: "msg-dm-perf-to-sec"}
message_get {message_id: "msg-dm-corr-to-sec"}
```

## Step 6: Orchestrator synthesizes a verdict

Combine all the findings into a single verdict:

```text
✓ SECURITY: SQL injection risk mitigated via parameterized queries (perf: +0.1ms).
✓ PERF: N+1 query fixed with batch fetch (estimated 1000x improvement on 1000 users).
✓ CORRECTNESS: Null check required before user_id access (guards against crashes).

Verdict: Approve with the three fixes above. All reviewers sign off.
```

Post the verdict to the room:

```bash
room_post {
  room: "code-review-panel",
  subject: "Verdict: Approve with fixes",
  body: "✓ SECURITY: SQL injection risk mitigated via parameterized queries (perf: +0.1ms).
✓ PERF: N+1 query fixed with batch fetch (estimated 1000x improvement on 1000 users).
✓ CORRECTNESS: Null check required before user_id access (guards against crashes).

Verdict: Approve with the three fixes above. All reviewers sign off."
}
```

## Recording checklist

For a screen recording, show:

1. **Room creation** — `room_create` call and confirmation.
2. **Parallel subagent spawning** — Launch 3 agents; show the prompt snippets they receive.
3. **Room filling up** — `room_history` shows the three parallel findings.
4. **DM exchange** — `inbox_read` for one subagent shows a DM from a peer.
5. **Selective body fetch** — Call `message_get` on one finding to show the two-tier model.
6. **Synthesis** — Orchestrator calls `room_post` with the verdict.
7. **Final room state** — `room_history` shows the full thread (3 findings + verdict).

**Timing:** ~2-3 minutes total (allow subagents 30-60 seconds to work in parallel).

## CLI parity

All MCP calls above have CLI equivalents (when `--features comms` is built):

```bash
# Room creation
basemind comms room-create code-review-panel --scope session --title "Code Review"

# Agent register (subagent-side)
basemind comms register --as-agent security --name "Security Reviewer"

# Room post (subagent-side)
basemind comms post code-review-panel "SQL injection risk in user lookup" \
  --as-agent security --body "Line 42 concatenates..."

# DM (subagent-side)
basemind comms dm correctness --to-agent correctness --as-agent security \
  --subject "Cross-check my SQL fix" --body "I flagged..."

# History (orchestrator-side)
basemind comms history code-review-panel

# Read message body (orchestrator-side)
basemind comms read msg-sec-1

# Inbox (subagent-side)
basemind comms inbox --as-agent security
```
