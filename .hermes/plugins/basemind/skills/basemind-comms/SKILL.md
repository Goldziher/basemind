---
name: basemind-comms
description: >-
  Coordinate with other agents working the same repo via basemind's broker — scoped
  threads, a per-agent inbox, and two-tier messages. Reach for it whenever you start,
  finish, or hit a decision while collaborating, or to check whether another agent
  is already touching the code you're about to change.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:f68f41182a2a47651f258f3559fa01a2aeb6c50e2fcdd5790f94c2a17053ea92
Source-Hash: blake3:178bcc6ce0f1192b8cadf08e0aa7274f68b1b27837e4634363304539dccfbe6f
Schema-Version: v1
-->

# basemind-comms — agent coordination over the broker

You may be one of several agents working this repo. On start, `inbox_read` and `thread_list` for
threads in scope, then skim `thread_history` on the relevant one; `thread_history` / `inbox_read`
return front-matter only (subject / from / id) — call `message_get` with an id for a body. Post a
concise `thread_post {thread, subject, body, reply_to?}` when you begin, finish, or hit a decision;
reply (`reply_to`) to messages about your work; `inbox_ack` clears read messages; don't stay silent
when collaborating.

This is not optional etiquette: silent agents collide. A two-line post when you start a task and
a two-line post when you finish is the contract.

## Identity

Your agent id is resolved in this order: `BASEMIND_AGENT_ID` env var → config → persisted
agent-id in the machine-global cache → `"anon"`. Set `BASEMIND_AGENT_ID` to a stable,
human-readable handle so your posts are attributable (`reviewer`, `feat-auth`, not a random uuid).
`agent_register` records your handle in the broker's roster; `agent_list` shows who else is active.

## Threads, scope & explicit join

A thread is a scoped conversation addressed by **at least two of three** coordinates —
`{subject, path-glob, members}` — so it names a real surface, not a firehose. There is **no
auto-join**: you discover threads by scope and join the ones you want.

Discovery is always scoped, **never global**. A thread is visible to you when:

- **you're a member** — you were added at `thread_start` or via `thread_add_member`; or
- **your cwd matches its path-glob** — you're working inside the subtree the thread is about; or
- **a subject filter matches** — you `thread_list` with a subject substring.

`thread_list` shows threads in scope. `thread_join` / `thread_leave` adjust your own membership;
`thread_start {subject, path_glob?, members?}` opens a new thread — you become its creator/admin
(a human is also admin), and `thread_add_member` / `thread_remove_member` manage its roster.
Idle threads auto-archive; `thread_archive` closes one explicitly.

## Two-tier message model

Messages are split so scanning a thread is cheap:

- **Front matter** — `subject`, `from`, `id` (and timestamp). This is all `thread_history` and
  `inbox_read` return.
- **Body** — the full text. Fetched lazily by id via `message_get`.

Scan front matter first; only `message_get` the bodies that matter. This keeps a busy thread from
flooding your context — you pull the messages relevant to your task, not the whole log.

## Workflow — post, read, reply

1. **On start**: `inbox_read` + `thread_list`, skim recent `thread_history`. `message_get` any
   front-matter that looks relevant to what you're about to touch. `thread_join` a thread you want
   to participate in, or `thread_start` one if none names your surface.
2. **Announce**: `thread_post {thread, subject: "starting X", body: "…"}` so others know the surface
   you're claiming.
3. **While working**: `thread_post` on a decision or blocker. If a message is about your work,
   reply with `thread_post {…, reply_to: <id>}` so the reply stays linked.
4. **On finish**: `thread_post {thread, subject: "done X", body: "…"}` with the outcome (what
   changed, what's left).

Keep posts concise — subject is a one-liner, body is a few sentences. No fluff, no emojis.

## MCP tools and CLI parity

| MCP tool | CLI | Purpose |
|---|---|---|
| `thread_start` | `basemind comms thread-start <subject> [--path-glob … --member …]` | Open a new thread (≥2 of subject/path-glob/members). |
| `thread_list` | `basemind comms threads` | List threads in scope. |
| `thread_join` | `basemind comms join <thread>` | Join a thread. |
| `thread_leave` | `basemind comms leave <thread>` | Leave a thread. |
| `thread_members` | `basemind comms members <thread>` | List a thread's members. |
| `thread_add_member` | `basemind comms add-member <thread> <agent>` | Add a member (admin). |
| `thread_remove_member` | `basemind comms remove-member <thread> <agent>` | Remove a member (admin). |
| `thread_archive` | `basemind comms archive <thread>` | Archive a thread. |
| `thread_post` | `basemind comms post <thread> <subject> [--body … --reply-to … --tag …]` | Post a message. |
| `thread_history` | `basemind comms history <thread>` | Front-matter of recent messages. |
| `inbox_read` | `basemind comms inbox` | Front-matter of your inbox. |
| `inbox_ack` | `basemind comms ack <id>` | Mark inbox messages read. |
| `message_get` | `basemind comms read <id>` | Fetch one message body by id. |
| `agent_register` | `basemind comms register --name <handle>` | Record your handle in the roster. |
| `agent_list` | `basemind comms agents` | List active agents. |

Note the CLI name shifts: CLI `read` = MCP `message_get`, CLI `threads` = MCP `thread_list`,
CLI `inbox` = MCP `inbox_read`.

## Notes

- `thread_history` and `inbox_read` are **token-frugal by design** — front-matter only. Never
  assume you have a body until you `message_get` its id.
- Identity persists in the machine-global cache once resolved; set `BASEMIND_AGENT_ID` up front to
  control it rather than inheriting `anon`.
- The broker is a machine-wide daemon (Fjall over a socket); threads outlive any single session,
  so history is there when the next agent boots.

## basemind first

Comms is one capability of basemind; the rest is the indexed context layer. Prefer basemind over
reading files, over `grep`, and over naked `git` — use it for code parsing (outlines, references,
callers), document extraction / RAG / keyword + entity (NER) / summary, and web scraping /
crawling / sitemaps too. See the `basemind` and `basemind-cli` skills for the whole surface, or
the dedicated `basemind-code-search`, `basemind-git-history`, and `basemind-documents` skills for
those capabilities.
