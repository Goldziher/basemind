# Agent comms v2 — specification

This is the normative specification of agent messaging in basemind. The decision and its reasons are
in [ADR-0021](adr/0021-agent-comms-v2.md); this document carries the detail and may change without a
new ADR as long as it stays inside that ADR's rules. Where the two disagree, the ADR wins.

**Implementation status.** The model is being built in `src/comms/v2/` on the branch
`feat/comms-v2-api` and is not on `main`. That code predates ADR-0021 in five places, where this
document wins: the post body limit (64 KiB there, 16 KiB here), the thread cap (refuses there,
archives here), expiry (judged at read time there, by the sweep only here), the clock-tolerance rule
(present there, absent here), and read positions (per member there, per actor here).

## Terms

- **Principal** — the identity a connection resolves to (see *Identity*).
- **Sub-identity** — `<principal>.<name>`, chosen per call with `as`.
- **Actor** — a principal or one of its sub-identities. Every call is made by exactly one actor.
- **Repository** — the registry's repository id when the root has one, else `path:<canonical root>`.
  Clones and worktrees of one repository share the id.
- **Head** — the `seq` of a thread's newest message.
- **Seen** — an actor is seen once any call it makes succeeds.

## Identity

### Resolution order

One function resolves the principal for `basemind serve`, the CLI and the hooks:

1. `BASEMIND_AGENT_ID`, verbatim.
2. The `comms.agent_id` config key.
3. The derived id `<host>-<repo>-<hash>`.

A value from tier 1 or 2 that fails the grammar is not used: resolution falls to the next tier and
the first agent-tool reply carries a notice naming the rejected value (cut to 160 bytes) and the id
in use.

### Derived id

| Part | Source | Rule |
|---|---|---|
| `<host>` | `BASEMIND_HOST` | lower-cased, reduced to `[a-z0-9]`, cut to 16 bytes; `agent` when unset or empty after that |
| `<repo>` | basename of the root | lower-cased, characters outside `[a-z0-9-]` replaced by `-`, cut to 24 bytes; empty for a root with no basename |
| `<hash>` | the root | first 6 hex digits of blake3 over the canonical absolute path |

The root must be absolute; a relative root is refused and never completed from the working
directory, which a hook does not share with its `serve`. The root is canonicalised with symlinks
resolved. When that fails (the path does not exist or cannot be read), `.` and `..` are folded
lexically and that path is hashed: a stable id, though not the canonical one when a symlink lies on
the way.

The plugin launchers and the plugin hooks set `BASEMIND_HOST`. Harnesses that leave it unset all
derive `agent-<repo>-<hash>` and share that identity within one checkout.

### Grammar

- A principal is at most 128 bytes and contains no `.`.
- A sub-name matches `[a-z0-9_-]{1,32}`.
- `.` is the separator of principal and sub-name and appears nowhere else.
- A composed id over 128 bytes is refused.

### Connection and call

The principal, the root and the host are sent once in the connection's `Hello` and are fixed for
that connection. Each call's envelope carries the optional sub-name. The daemon builds the caller
(actor, repository, replay key) from those; nothing in a request body names the sender.

### Hook-injected `as`

In Claude Code the plugin's `PreToolUse` hook matches the three agent tools and, when the call has
no `as`, injects one through `updatedInput`:

- inside a subagent: `<agent_type>-<8 hex of blake3(agent_id)>`, the type lower-cased, reduced to
  the sub-name grammar and cut to 23 bytes;
- in the main session: `session-<8 hex of blake3(session_id)>`.

The inbox hook reads with the same name. An orchestrator that wants to address a particular
subagent tells it to pass an explicit `as`, since an injected name cannot be predicted.

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
| Body returned inline | 4 KB |
| Response budget of one inbox reply | 16 KB |
| `limit` | default 20, maximum 100 |
| `wait_secs` | 0 to 40 |

A value over a limit is refused, never cut, with two exceptions: a `limit` above 100 means 100 and a
`limit` of 0 means 1. A `to` that names only the creator is refused. A join or invitation that would
take a thread past 256 members is refused whole.

### Thread cap

When a repository already holds 256 active threads, creating another archives the least recently
active one: the thread whose newest post has the lowest position in the store-wide post order. The
post that caused it succeeds, and its reply carries a notice naming the archived thread.

## Membership and read positions

- Members are principals. The creator's principal and every `to` recipient are members from
  creation. A sub-identity named in `to` or `invite` makes its principal the member.
- `to` may name an agent that has never connected. The reply lists every recipient that is not seen
  under `pending`; this cannot tell a mistyped id from an agent that has not started yet.
- Each actor has its own read position (`read_seq`) and announcement mark (`notified_seq`) in every
  thread its principal is a member of. Both start at the thread's head as of the later of two
  moments: the actor being first seen, and its principal becoming a member. An actor seen before a
  thread was created therefore starts that thread at 0.
- A read position never moves back, and never moves across an unread message that was not returned.
- A seen record is written on an actor's first successful call and rewritten at most once a day.

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
| `peek` | default `false`; `true` reads unread messages without marking |
| `wait_secs` | wait up to this long when nothing is unread |
| `limit`, `cursor` | paging |

- A marking read (`unread: true`, `peek: false`) returns the oldest unread messages in store-wide
  post order and advances `read_seq` and `notified_seq` to the last message returned per thread.
- A body over 4 KB is returned cut with `truncated: true` and counts as read.
- The budget counts, per message, the bytes of the body as returned, the subject, the sender and
  the id. The first message is always returned; delivery stops at the first later one that does not
  fit. What is left stays unread and the reply has `more: true`.
- A marking read takes no `cursor` (the read position is its cursor). `peek` and history reads page
  with `next_cursor`.
- Refused combinations: `thread`, `limit`, `cursor`, `peek`, `unread` or `wait_secs` beside `id`;
  `wait_secs` beside `unread: false`.

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
- an archived thread stays readable to its members and can be left; posting, joining and inviting
  are refused;
- when the last member leaves, the thread is archived in the same step.

### Waiting

The store's read never blocks. When a `peek` or marking read finds nothing unread and `wait_secs` is
above 0, the daemon waits for the store-wide post counter to change, reads again, and gives up after
`wait_secs`. The caller's own posts end no wait. The daemon checks that the client is still
connected immediately before a marking read, so a waiter whose client has gone advances nothing. A
waiting read is exempt from the 10-second request bound of ADR-0013.

## Cursors

A cursor is opaque. It is bound to the actor, the selection (every thread or one; for a listing, the
repository and filter) and the read mode. A cursor that is malformed, or was issued to another actor
or for another read, is refused as invalid.

## Replay

- The stdio relay stamps every agent-tool request with a random key and re-sends the same key when
  it replays the request after a daemon restart. No tool has an idempotency field.
- A post into a thread is deduplicated on (actor, thread, key); a post that creates a thread on
  (actor, key), so a replay returns the thread already created.
- A marking read stores the range it delivered against the key, so a replay returns that range
  again. A read that moves nothing stores nothing.
- The daemon registers a key as in flight before it starts a wait, so a replay that arrives during
  the wait joins it.
- The first write for a key wins. The sweep removes replay entries older than one hour.
- A model that repeats a call sends a new request with a new key and makes a second message.
- CLI and hook calls have no relay and no key. The hooks therefore read with `peek` and advance only
  `notified_seq`; a CLI read that marks is unprotected.

## Time and order

- A timestamp is `max(clock, previous timestamp + 1 microsecond)`.
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
| Seen record | written more than 31 days ago |
| Replay entry | written more than one hour ago |
| Legacy `store.legacy-<date>` directory | 30 days after the date in its name |

A thread's last activity is the latest timestamp of any post made to it. A sweep works within a
budget and continues on the next run. No read hides a record that the sweep has not removed.

## Store

- Engine: Fjall. Directory: `agents.fjall` inside the daemon's comms directory (ADR-0024).
- Version: `COMMS_STORE_VER`, starting at 1000, stored in the store's metadata. It is independent of
  the release version.

| Store on disk | Behaviour at open |
|---|---|
| none | created at `COMMS_STORE_VER` |
| older version | migrated by forward-only steps, each committed together with its new version |
| same version | opened |
| newer version | opened; every write is refused with `store_newer`. Peeks, history, listings and reads by id work; marking reads and hook reads do not |
| unreadable, or missing part of its layout | not opened and not modified; reported by `basemind agent status` |

basemind never deletes, recreates or completes an `agents.fjall` directory. Recovery from an
unreadable store is manual: stop the daemon, then rename or remove the directory.

On first start the legacy `store.fjall` directory is renamed to `store.legacy-<UTC date>` (with
`-2`, `-3` and so on when the name is taken). Nothing is imported from it.

A change is committed as one Fjall batch with buffered persist: it survives a daemon crash, and a
power loss can lose the most recent changes. A `seq` lost that way is issued again.

## Notices

Every reply has `notices`, a list of sentences about something other than the request:

- a rejected `BASEMIND_AGENT_ID` or `comms.agent_id` value, on the first reply;
- more than one live `serve` session for the principal, with the advice to pass `as` or set
  `BASEMIND_AGENT_ID`. One-shot CLI and hook connections are not counted;
- a thread archived by the thread cap, on the reply of the post that caused it.

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
