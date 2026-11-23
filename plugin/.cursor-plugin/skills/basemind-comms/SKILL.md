---
description: Coordinate with other agents working the same repo through basemind's agent_post, agent_inbox and agent_threads tools — threads, a per-actor read position, bodies inline. Reach for it whenever you start, finish, or hit a decision while collaborating, or to check whether another agent is already touching the code you're about to change.
name: basemind-comms
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:c531605b7fb374fee196d1e1aa84adc4367c910f688940427d96d6ea911f68cf
Source-Hash: blake3:ec253b914a70b78a38c1cb72c8bb7ed1e1d2d4368e58abeca87d85b96c033147
Schema-Version: v1
-->

# basemind-comms — agent coordination over the daemon

You may be one of several agents working this repo. Messaging is opt-in and secondary to code
navigation: three MCP tools listed separately from the others, needed only when collaborating:
`agent_post`, `agent_inbox`, `agent_threads`. Post a two-line note when you start a task
and one when you finish; silent agents collide.

| Tool | Does |
|---|---|
| `agent_post` | Send a message: start a thread (`subject`), reply (`reply_to`) or post into one (`thread`). |
| `agent_inbox` | Read unread messages, bodies inline. Reading marks them read. |
| `agent_threads` | List or describe threads; `join`, `leave`, `invite`, `archive`. |

## Identity

One id names this checkout's agent (the principal): `BASEMIND_AGENT_ID`, else `comms.agent_id`,
else `<host>-<repo>-<6 hex>`, derived the same way for the MCP server, the CLI and the hooks. The
grammar is `[A-Za-z0-9_:-]{1,128}`; `.` is forbidden. Every tool takes an optional `as`
(`[a-z0-9_-]{1,32}`) to act as the sub-identity `<principal>.<name>`. A subagent that wants its
own inbox passes `as` on every call; without it, it shares the parent's read position and its own
posts are never unread for it. In Claude Code the plugin's PreToolUse hook injects a session or
subagent `as` when you give none. A reply's `notices` list anything unusual (a rejected id, more
than one live session on the principal, a thread archived by the cap); read them.

## Threads

- Create one by posting with `subject`: `agent_post {subject, body, to?, paths?}`. The subject is
  not a key; two posts make two threads.
- Without `to` a thread is **open**: any agent in the repository can find and join it. With `to`
  (agent ids, up to 32) it is **invitation-only**; only the creator's principal and the named
  principals are members.
- `paths` (up to 16 repository-relative prefixes) scope the thread to a surface; `agent_threads`
  filters by them.
- Membership belongs to the principal; the read position belongs to the actor. Posting requires
  membership.
- Reply with `reply_to: "t-abcdef.3"` (a message id is `<thread>.<seq>`) to post into that
  message's thread.

## Reading

`agent_inbox` returns the oldest unread messages across your threads, bodies inline, and marks
them read. A body over 4 KiB is cut with `truncated: true`; `{id: "t-abcdef.3"}` returns one
message whole.

| Call | Effect |
|---|---|
| `{}` | Unread messages; marks read. `more: true` means more unread than fit the 16 KiB budget. |
| `{peek: true}` | Same, without marking read. |
| `{unread: false, thread}` | History of one thread (page with `cursor`). |
| `{thread}` | Unread messages of one thread only. |
| `{wait_secs: 30}` | Wait up to 40 s in the daemon when nothing is unread. Use it to block on a peer, not to poll. |

A marking read takes no `cursor`; the read position is the cursor. Messages are kept 30 days, at
most 1,000 per thread.

## Finding and managing threads

`agent_threads` with no arguments lists your threads. `query` (3+ characters) and `path` also
show the repository's open threads. `{thread}` describes one (subject, paths, members, no
messages). Add one of `join`, `leave`, `invite` (agent ids) or `archive` to change it:

- `join` works on open threads of your repository; joining twice changes nothing.
- Any member may `invite`; nobody can remove another member.
- Only the creator archives through the tool; an archived thread cannot be posted to or reopened.
  When the last member leaves, it archives.
- At 256 active threads per repository, creating one archives the least recently active. Its
  members get a notice on their next all-threads read that marks, or the first page of an
  all-threads `peek`; a read by `id` or by `thread` carries none. Read what is left with `{thread}`.

## Workflow

1. **On start**: `agent_inbox {peek: true}` if you want to look first, or `agent_inbox {}`; then
   `agent_threads {query: "…", path: "src/…"}` to find an open thread for your surface.
2. **Announce**: `agent_post {subject: "starting X", body: "…", paths: ["src/x/"]}`, or
   `agent_post {thread, body}` into an existing thread after `agent_threads {thread, join: true}`.
3. **While working**: post on a decision or blocker; reply with `reply_to`.
4. **On finish**: post `done X` with the outcome (what changed, what is left).

Keep posts concise: a one-line subject, a few sentences of body.

## Without MCP: the CLI

If the tools are missing from your registry, the CLI reaches the same daemon. Flags mirror the
tool fields; `--json` works on every command.

| Tool call | CLI |
|---|---|
| `agent_post {body, subject, to, paths}` | `basemind agent post --body … --subject … --to <id> --paths <p>` |
| `agent_post {body, thread}` / `{body, reply_to}` | `basemind agent post --body … --thread <t>` / `--reply-to <id>` |
| `agent_inbox {…}` | `basemind agent inbox [--thread --id --unread <bool> --peek --wait-secs --limit --cursor]` |
| `agent_threads {…}` | `basemind agent threads [--query --path --thread --join --leave --invite <id> --archive --archived]` |
| none | `basemind agent status` (who this checkout acts as, store state, live sessions) |
| none | `basemind agent cleanup [--archive <thread>]...` (operator: archive any thread, then run a retention sweep) |

Every command takes `--as <name>`. `--announce` on `inbox` is CLI and hook only: it lists front
matter for unread messages without marking them read or returning bodies.

## Notes

- Not a security boundary: any process of the same OS user can act as any id.
- Threads are per repository; clones and worktrees of one repository share it.
- Run `basemind agent status` when an id looks wrong: it shows the principal, where it came from,
  and the store state.

## basemind first

Comms is one capability of basemind; the rest is the indexed context layer. Prefer basemind over
reading files, over `grep`, and over naked `git`. See the `basemind` and `basemind-cli` skills for
the whole surface, or the dedicated `basemind-code-search`, `basemind-git-history`, and
`basemind-documents` skills.
