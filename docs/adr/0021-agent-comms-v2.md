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
   clock or file. `<host>` is `BASEMIND_HOST` lowercased, reduced to `[a-z0-9]` and cut to 16
   bytes, or `agent` when that leaves nothing. `<repo>` is the root directory's basename,
   lowercased, with every character outside `[a-z0-9-]` replaced by `-`, cut to 24 bytes. Both are
   sanitised before they are cut, so both are ASCII. A root with no basename, such as `/`, has an
   empty `<repo>`, and its id reads `<host>--<hash>`.

The root is an absolute path, and `serve`, the CLI and the hooks take it from one shared discovery
function. A relative root is refused with a typed error and is never completed from the working
directory, which a hook does not share with its `serve`. The root is canonicalised with symlinks
resolved. When that fails, because the path does not exist or cannot be read, its `.` and `..` are
folded lexically and the result is hashed instead: a stable id, though not the canonical one when
a symlink lies on the way.

The plugin launcher and the plugin hooks set `BASEMIND_HOST` from one shared script, and a test
asserts that the hook identity equals the `serve` identity. The `session-*`, persisted `agent-id`
and generated `agent-*` tiers are removed, and so is the identity collision ledger (`CLAIMS_DIR`,
`CLAIM_TTL`). Identity is trusted per operating-system user; there is no credential.

An id from the environment or config that fails validation is not used. The resolver falls to the
next tier and logs a warning, and the first agent-tool reply carries a notice naming the rejected
value, cut to 160 bytes, and the id in use.

**Sub-identities.** A subagent passes `as: "<name>"` and acts as `<principal>.<name>`, so a session
can only act as its own sub-identities. `<name>` matches `[a-z0-9_-]{1,32}`. `.` is reserved as the
separator, so a principal id containing it fails validation. A composed id over the 128-byte
`MAX_ID_LEN` is refused.

The server cannot tell a subagent from its parent, because both use one MCP connection, so it cannot
require `as`. In Claude Code the plugin's `PreToolUse` hook, whose matcher is widened to the three
agent tools, sees `agent_type` and `agent_id` inside a subagent. When the caller did not set `as`
it injects `<agent_type>-<8 hex of blake3(agent_id)>` through `updatedInput`. The type is
lowercased, reduced to the sub-name grammar and cut to 23 bytes, which is what the hash leaves of
the 32; the hash is of the whole `agent_id`, because two ids may share their first bytes. Such a
name cannot be predicted, so an orchestrator that wants to address a subagent tells it to pass an
explicit `as`. On other hosts the tool description is the only guard.

**Caller.** Who is calling is derived by the daemon and never read from a request. The principal
comes from the session's `Hello`, the sub-identity from the `sub` of the `Call`, the repository from
the session's root, and the replay key from the relay. A request body holds only what the client
chose: the target and text of a post, the selection of a read, the filter or change of a thread
call. A client therefore cannot post as another agent or from another repository, whatever it
sends.

**Shared identity.** Two sessions of one host in one checkout resolve to the same id and are not
split. The daemon counts the live `serve` relay sessions per principal; one-shot CLI and hook
connections do not count. While more than one is live, replies from all three tools carry a
notice that another session shares the identity and that `BASEMIND_AGENT_ID` or `as` separates
them.

**Notices.** Every reply has `notices`, a list of sentences that are not about the request. The
store produces none. The daemon appends the shared-identity notice, since only it counts sessions,
and the tool layer appends the rejected-id notice, since only it knows what the resolver refused.

**Threads and messages.** A thread id is `t-<6 base32>`, checked for collision at creation and
drawn again, at most 64 times; a post that finds no free id in that many draws fails as a store
error and stores nothing. A message id is `<thread>.<seq>`, so a reference is a point lookup. A
thread is created by a post that names a `subject` and no `thread`; the subject is not a key, so
two such posts make two threads. A thread belongs to one repository: the registry `RepoId` when
there is one, else `path:<canonical root>`. Two clones of one remote share a `RepoId` and
therefore share threads, by design. A thread is open to its repository unless created with `to`,
which makes it invitation-only. Its `paths` are repo-relative path prefixes, fixed at creation.

A subject is one line of at most 200 bytes and a body at most 64 KiB. `to` and `invite` name at most
32 agents each (`MAX_RECIPIENTS`) and `paths` at most 16 prefixes (`MAX_THREAD_PATHS`). A value over
a limit is refused, never cut, and so is a `to` that names only the creator, which would make a
thread nobody else can reach. A thread has at most 256 members (`MAX_THREAD_MEMBERS`): a join, or
an invitation of agents that are not members yet, is refused whole when it would pass that.

**Membership and read position.** One table, `members(thread, agent, read_seq, notified_seq)`, with
a reverse `member_threads` index, replaces `thread_members`, `thread_subs`, `Thread.members` and the
`cursors` keyspace. The creator and every `to` recipient are members from creation and start at
sequence 0. `to` may name an agent that has never connected, since ids are deterministic; that agent
finds the thread on its first `agent_inbox`. The reply lists every recipient the store has not
seen under `pending`, so a mistyped id is visible. An agent that joins starts at the head, and an
invited agent at the head as of the invitation. `pending` cannot tell a typo from an agent that has
not started yet; it only makes both visible. `notified_seq` is the inbox hook's high-water mark
(what it has already announced), kept in the store so the hook no longer keeps a file of its own.

An agent is seen once any call it makes succeeds, a hook's read included. Being named in `to` or
`invite` does not make it seen, and neither does a refused call. An agent's first call writes its
seen record at once; after that the record is rewritten at most once a day
(`SEEN_REFRESH_INTERVAL`), so that a read does not write on every call. A record stops counting 31
days after it was written (`SEEN_TTL` plus that day), and the agent is `pending` again. An agent is
therefore never `pending` within 30 days of a call, and is `pending` again at most a day after
that.

Only an unread read marks. It advances `read_seq` to the last message returned in each thread, and
`notified_seq` with it, so what was read is not announced afterwards; there is no separate
acknowledgement. `unread: false` reads history and moves neither `read_seq` nor
`notified_seq`, and `peek` moves nothing either. `read_seq` never advances across an unread message
that was not returned: a reply is always the oldest unread messages, in order, and stops where the
limit or the budget stops it.

Messages come in the order they were posted to the store, across threads. The store therefore
keeps one monotonic post counter, and that counter also orders an inbox that spans threads.

A message's timestamp is the clock's reading. While the clock is not later than the previous
message's timestamp and no more than a minute behind it, the timestamp is one microsecond past the
previous one instead, so a clock that stands still or steps back slightly does not stamp two posts
alike. Once the clock reads more than a minute behind the previous timestamp, that timestamp is
taken to be the wrong one and posts are stamped from the clock again. Timestamps therefore do not
order messages: `seq` does within a thread and the post counter across threads, and read positions
and cursors use nothing else. The previous timestamp is read for the next stamp and for a warning
the store logs when the clock is more than a minute behind it, at open and when it stamps a post.
Expiry never reads it: message, replay and seen expiry are judged against the clock as it reads.
A thread's last activity is the latest timestamp of any post made to it. It never moves back: a
post stamped earlier than the thread's last activity leaves it where it is.

A wrong clock is not detected, only survived. While the clock reads far ahead:

- everything older than its retention by that reading is hidden, and what a sweep reaches in that
  time is deleted for good. What no sweep reached is visible again once the clock is corrected;
- threads hidden that way do not count against the cap of their repository, so the repository can
  hold more than 256 active threads once they are visible again. None is removed for it; new ones
  are refused until the count is under the cap;
- a message posted then keeps its stamp and expires when the clock passes the stamp by
  `MESSAGE_TTL`. Its thread keeps that stamp as its last activity, and a post after the
  correction does not put the thread back on the clock: a thread created or posted to then is
  listed first and holds its place under the cap, unless it is archived, until the clock passes
  the stamp by `MESSAGE_TTL`. A thread nobody posted to in that time is not affected and expires
  `MESSAGE_TTL` after its own last post;
- seen records and replay entries written then live until the clock passes their own time: such
  an agent is not `pending` until 31 days past the stamp unless it calls again, which rewrites
  its record at the clock, and such a replay key answers with its first result until an hour past
  the stamp.

A clock that ran behind is the mirror image for messages, seen records and replay entries: what
was stamped then expires early by the difference. A thread is the exception, because its last
activity does not move back: a message stamped behind expires early alone, and the thread and the
messages stamped before it stay as long as they would have. If a thread's last activity followed
the last post instead, one post stamped 31 days behind would expire the thread and every message
in it as soon as the clock was right again.

Neither can reorder messages, make a cursor skip or repeat one, issue a `seq` twice, or hold posts
made after the correction in the future. Nor can a read position pass an unread message that a
read would return. A message hidden as expired while the clock read ahead is not returned, though,
and a read that marks in that time can advance past it: when the clock is corrected the message is
back in the history and is not unread.

A body
over 4 KB is returned cut, with `truncated: true`, and counts as read (the full body comes from
`id`). The 16 KB response budget counts, for each message, the bytes of its body as returned, its
subject, its sender and its id. The first message is always returned, and delivery stops at the
first one after it that does not fit, even when a later one would. What is left out stays unread
and the reply says `more: true`. `limit` defaults to 20; a larger value than `MAX_LIMIT = 100` means
100, and 0 means 1.

An unread read that marks takes no `cursor`: the read position is its cursor, and passing one is
refused naming the field. `peek` and `unread: false` reads page with `next_cursor`. A cursor is
opaque and bound to the agent, the selection (every thread, or one) and the read mode; one that is
malformed, or was issued to another agent or for another read, is refused as an invalid cursor.
A call cancelled after the advance loses that page from the unread view; `unread: false` reads it
again.

**Idempotency and replay.** No tool has an idempotency field. The relay stamps each `Post` and each
`InboxRead` with its existing random per-request key (`relay-<key>`, `src/stdio_relay.rs`) and
re-sends the same key on a replay. A `Post` into a thread dedupes on `(from, thread, key)` as today.
A `Post` that creates a thread dedupes on `(from, key)`, so a replay returns the thread already
created. An `InboxRead` that moves a position stores the delivered range against the key, so a
replay returns the same range, as stored, instead of an empty page; a read that moves nothing, an
empty one included, stores nothing, because repeating it is harmless. Dedupe entries and stored
ranges live for one hour (`REPLAY_TTL`), and the first write wins: a later call with the same key
never replaces an entry that still answers. An entry past its hour no longer answers, whether or
not it has been deleted yet, and the next call with its key replaces it.
A model that repeats a call sends a new request and makes a second message. A CLI or hook
call has no relay and no key, so it has no replay protection; the hooks therefore read without
marking as read and advance only `notified_seq`.

**Waiting.** The store's read never blocks. `wait_secs` belongs to the daemon: when a `peek` or a
marking read finds nothing unread, the daemon waits for the store's post counter to change, looks
again, and gives up after `wait_secs`. The caller's own posts move the counter and end no wait,
because they are never unread to it. A read by id, a history read and the hooks' read do not wait,
and `wait_secs` beside `id` or `unread: false` is refused.

Two rules keep a wait from losing messages. The daemon checks that the client is still connected
immediately before the marking read, so a waiter whose client is gone does not advance the read
position. And it registers the replay key as in flight before it starts to wait, so a replay that
arrives while the first attempt is still waiting joins it instead of starting a second read.

**Authorization.** All rules live in `src/comms/v2/rules.rs`, as functions every store
implementation calls:

- reading and posting require membership;
- joining requires an open thread in the caller's repository;
- any member may invite, and may invite an agent of another repository: membership crosses
  repositories by invitation only. No member can be removed by another;
- only the creator may archive, whether or not it is still a member;
- a thread is described (subject, paths, peers, no messages) to its members, and to any agent of
  its repository while it is open;
- a listing with no filter returns the caller's own threads, from every repository;
- a listing with a `query` of at least three characters or a `path` returns the caller's own
  threads plus open threads of the caller's repository, in both cases only those that match. With
  both, a thread must match both. A `path` matches a thread when it is a prefix of one of the
  thread's `paths`, or one of them is a prefix of it.

An archived thread stays readable to its members and can be left; posting, joining and inviting
are refused. Joining a thread one is already in changes nothing, archived or not. When the last member leaves, the thread is archived in the same step.

A listing is ordered by last activity, newest first, and pages by an opaque cursor over
`(last activity, thread id)` that is bound to the agent, its repository and the filter. A thread
that receives a post between two pages moves ahead of the cursor and can be missing from the
remaining pages; a new listing shows it first, unless another thread carries a last activity ahead
of the clock. No post moves a thread the other way, so no thread is listed on two pages.

**Tools.** Three tools under the ADR-0018 contract replace `agents`:

| Tool | Fields |
|---|---|
| `agent_post` | `body`; one of `thread`, `reply_to`, `subject`; `to?`, `paths?`, `as?` |
| `agent_inbox` | `thread?`, `id?`, `unread=true`, `peek=false`, `wait_secs` 0–40, `limit=20`, `cursor?`, `as?` |
| `agent_threads` | `query?`, `path?`, `thread?`, `join?`, `leave?`, `invite?`, `archive?`, `archived?`, `limit`, `cursor?`, `as?` |

`agent_inbox` returns bodies inline and marks what it returns as read unless `peek` is set.
`agent_threads` with no arguments lists the caller's threads and the peers in them;
`{thread, archive: true}` archives a thread the caller created. A call carries at most one of
`join`, `leave`, `invite` and `archive`; more than one is refused with an error naming the fields.
The tool is not annotated read-only.

The fields are checked in one place, shared by the tools, the CLI and the hooks, and a field that
does not apply to a call is refused, never ignored: `thread` beside `id`; `limit`, `cursor`, `peek`
or `unread` beside `id`; a change without `thread`; `query`, `path`, `cursor`, `archived` or `limit`
beside a change or beside a `thread` to describe. An empty string for an optional field is the
field left out, and a field passed with its default value counts as left out too. `peek` beside
`unread: false` is accepted, since both ask for nothing to be marked.

The three tools are not always-loaded: they stay deferred, outside `VERB_TOOLS`, under their own
5,000-byte ceiling with its own test. The 20,000-byte verb ceiling is unchanged.

**Removed.** `register`, `ack`, `remove_member`; the `workspace` tool; worktree claims and their
release, everywhere. `cleanup` and `status`, and the read-only `workspaces`, `worktrees` and
`branches` listings, remain as CLI commands only.

**Storage.** The store stays on Fjall, in `agents.fjall` inside the comms directory.
`COMMS_STORE_VER = 1000` replaces `COMMS_SCHEMA_VER` and is no longer tied to `RELEASE_MINOR`, so
a release does not wipe comms. Later shape changes are
forward-only numbered migration steps run at open. A store newer than the binary is refused for
writing with a message that names the fix, and is never wiped. Nothing is recorded on it, a seen
record included, and the hooks' read is refused because it writes `notified_seq`. What only reads
still works: a `peek`, history, a listing and a read by message id. What has expired is not
returned there either.

A store that cannot be opened, holds rows but no version, holds a version or a counter that cannot
be read, lacks a keyspace of its layout or has no migration path is a typed error
(`CommsError::Backend`) that names the directory, and is left untouched: basemind never deletes, recreates or completes one, and creates keyspaces only where
there is no store. Opening does let Fjall recover its journal and compact, on a newer store too,
which rewrites files but no key or value.

A change is committed whole or not at all, and is handed to the operating system before the call
returns. It is synced to disk on the daemon's periodic tick and at shutdown. A crash of the daemon
therefore loses nothing; a loss of power can lose what was committed since the last sync. A `seq`
lost that way is issued again to the next post in its thread.

The legacy store is not imported. On first start the legacy `store.fjall` directory is renamed to
`store.legacy-<date>` inside the comms directory (the UTC date, with `-2`, `-3` and so on appended
when that name is taken), basemind never deletes it, and a fresh store is created. An import would
carry little: most agent ids in it are per-process, messages older than
three days are already deleted, and its threads carry no repository.

**Retention.** Messages are kept 30 days (`MESSAGE_TTL`, three today) and at most 1000 per thread
(`MAX_MESSAGES_PER_THREAD`, unchanged). Messages expire one by one, and the unread count is of
what has not expired. A thread expires `MESSAGE_TTL` after its last activity, archived or not, and
every message it still has goes with it.
Archiving is by the creator, or automatic when the last member leaves; the idle auto-archive is
removed, and with it `THREAD_IDLE_TTL` (seven days) and `THREAD_RETENTION_TTL` (14 days). Threads
now stay active for up to 30 days, so the machine-wide `MAX_ACTIVE_THREADS = 256` becomes a cap per
repository.

Expiry is judged when a record is read, against the clock; deleting is a separate sweep with a
budget, run after changes and on the daemon's tick. A record hidden as expired is therefore
visible again if the clock goes back, unless a sweep already removed it. A thread can come back
that way with fewer messages than its `head` counts, which a read or a cursor treats like
messages the per-thread cap dropped.

**Wire.** `PROTO_VER` goes from 5 to 6: `Hello` gains the host and root beside its existing
`remote` and `cwd`, `Call` gains `sub` for the sub-identity, and `Post`, `InboxRead`, `ThreadQuery`
and `ThreadUpdate` replace the twenty coordination requests and the two claim requests in
`src/comms/protocol.rs`. The wait runs in the daemon, around the store's read, when `InboxRead`
asks for one. `RELAY_PROTO_VER` goes from 1 to 2.

A refusal travels as its `code`, its fields and its rendered `message`. A binary that does not
know a code keeps the message and shows it, so a new refusal code does not need a `PROTO_VER` of
its own.

**Amendments to ADR-0013.** Protocol v4 becomes v6. The 40 s cap on `wait` becomes the upper bound
of `wait_secs`, and an `InboxRead` that waits is exempt from the 10 s request bound. `InboxRead` is
replayed after a daemon restart, against the stored delivered range. Post idempotency keeps its
one-hour window; the key is always the relay's and never a caller field, and a thread-creating post
dedupes on `(from, key)`.

## Consequences

- Breaking change to the MCP tools, the CLI, the wire protocol and the store layout, taken in one
  release with the hooks, skills and docs. No mode aliases are kept, and existing threads and
  messages are not carried over.
- `src/mcp/mode.rs`, `src/mcp/lenient.rs` and `DOMAIN_TOOLS_BYTE_CEILING` are deleted, completing
  ADR-0020. A session pays nothing for the agent tools until a host loads them through tool search.
  In a host that defers tools, discovery relies on the server instructions and the session-start
  hook naming the three tools.
- CLI parity (ADR-0014): the 18 `agents` rows and 5 `workspace` rows in
  `tests/cli_parity/capabilities.rs` are replaced by three injective tool rows, and the
  mode-to-subcommand bijection disappears with `domain_modes()`. The kept listings are CLI commands
  with no MCP tool, which ADR-0018 allows.
- Individual-tier memory is keyed by agent id (`src/mcp/memory.rs`), so it restarts empty under the
  new ids. Most sessions lose nothing, because their `session-*` id already changed on every start.
  Two sessions that share an identity now share individual-tier memory.
- `agent_inbox` is a mutation. A response lost after the read leaves those messages read; a call
  cancelled while it waits does not, because the daemon checks the connection before it reads.
- A thread listing paged while threads receive posts can miss one; nothing unread is lost by it,
  since the inbox does not depend on the listing.
- After a loss of power a message id a client still holds can name a different message than it
  did, because the `seq` it carries was issued again.
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
