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
Content-Hash: blake3:5853fcdee0907f6aa81e9e3f7648c702299e19aaa4729a7a1a728859b483d291
Source-Hash: blake3:85c432430a8315da4f7225ca2a6f5de96b425254183a6ea753e09b49c4846455
Schema-Version: v1
-->

# basemind-comms — agent coordination over the broker

You may be one of several agents working this repo. On start, call `agents` mode `inbox` and
`thread_list`, then skim mode `history` on the relevant thread. `history` / `inbox` return
front-matter only (subject / from / id) — use mode `message` with an id for a body. Post with
`agents { mode: "post", thread, subject, body, reply_to? }` when you begin, finish, or hit a
decision; mode `ack` clears read messages. Don't stay silent when collaborating.

This is not optional etiquette: silent agents collide. A two-line post when you start a task and
a two-line post when you finish is the contract.

## Identity

Your agent id is resolved in this order: `BASEMIND_AGENT_ID` env var → config → persisted
agent-id in the machine-global cache → `"anon"`. Set `BASEMIND_AGENT_ID` to a stable,
human-readable handle so your posts are attributable (`reviewer`, `feat-auth`, not a random uuid).
`agents` mode `register` records your handle in the broker's roster; mode `list` shows who is active.

## Threads, scope & explicit join

A thread is a scoped conversation addressed by **at least two of three** coordinates —
`{subject, path-glob, members}` — so it names a real surface, not a firehose. There is **no
auto-join**: you discover threads by scope and join the ones you want.

Discovery is always scoped, **never global**. A thread is visible to you when:

- **you're a member** — you were added with mode `thread_start` or `add_member`; or
- **your cwd matches its path-glob** — you're working inside the subtree the thread is about; or
- **a subject filter matches** — you call `agents` mode `thread_list` with a subject substring.

`agents` mode `thread_list` shows threads in scope. Modes `join` / `leave` adjust your membership;
mode `thread_start` opens a new thread — you become its creator/admin (a human is also admin), and
modes `add_member` / `remove_member` manage its roster. Idle threads auto-archive; mode `archive`
closes one explicitly.

## Two-tier message model

Messages are split so scanning a thread is cheap:

- **Front matter** — `subject`, `from`, `id` (and timestamp). This is all modes `history` and
  `inbox` return.
- **Body** — the full text. Fetched lazily by id via mode `message`.

Scan front matter first; only fetch the bodies that matter with mode `message`. This keeps a busy thread from
flooding your context — you pull the messages relevant to your task, not the whole log.

## Workflow — post, read, reply

1. **On start**: call `agents` modes `inbox` + `thread_list`, then skim `history`. Fetch any relevant
   body with mode `message`. Join a thread with mode `join`, or use `thread_start` if none names your surface.
2. **Announce**: `agents { mode: "post", thread, subject: "starting X", body: "…" }` so others know the surface
   you're claiming.
3. **While working**: use mode `post` on a decision or blocker. If a message is about your work,
   pass `reply_to: <id>` so the reply stays linked.
4. **On finish**: post with `subject: "done X"` and the outcome (what
   changed, what's left).

Keep posts concise — subject is a one-liner, body is a few sentences. No fluff, no emojis.

## Polling — check for replies while you work

Posting is half the contract. The other half is **reading replies before they go stale**: a peer
that answers your question, hands you a finding, or claims a file you were about to edit is only
useful if you notice within the task, not after it. An agent that posts once and then works silently
for twenty minutes is as bad as one that never posts.

Poll on a rhythm, not just at the start:

- **Every few minutes during long work**, and **always** at a natural checkpoint — before you start
  editing a new area, before you commit, and when a build or test run is doing the waiting for you.
- `agents` modes `inbox` and `history` are front-matter only, so a poll costs almost nothing. Only
  use mode `message` for ids whose subject actually concerns you.
- Acknowledge handled ids with mode `ack` so the next poll shows a real delta instead of the same
  backlog.

Track the ids you have already seen and surface only **new** ones — otherwise every poll re-reports
the whole thread and you learn nothing. If your harness can run a background watcher, poll on an
interval (~60s is a good default) and have it emit only unseen ids, filtering out your own posts:

```bash
# Emit each NEW message on a thread; ignore your own. Prime `seen` first so you
# only hear about what arrives from now on.
TH=<thread-id>; ME=<your-agent-id>; seen=$(mktemp)
basemind agents history "$TH" | awk -F'\t' '{print $NF}' > "$seen"
while true; do
  basemind agents history "$TH" | while IFS=$'\t' read -r subject from ts id; do
    [ -z "$id" ] && continue
    grep -qF "$id" "$seen" && continue
    echo "$id" >> "$seen"
    [ "$from" = "$ME" ] && continue
    echo "NEW from ${from}: ${subject} [id=${id}]"
  done
  sleep 60
done
```

Then `basemind agents message <id>` (MCP: `agents` mode `message`) only for bodies worth reading.

### When the MCP tools aren't there, use the CLI

If the basemind MCP tools are missing from your registry — the server failed to start, the schema
was rejected, the client dropped them — **coordination still works over the CLI**. Every `agents`
mode has a CLI twin (table below), so `basemind agents thread-list / history / message / post` gets
you a full conversation with no MCP. Don't conclude you are working alone just because the tools
didn't load; check the CLI before assuming silence. Use the `basemind-doctor` skill to diagnose why.

## MCP tools and CLI parity

| MCP call | CLI | Purpose |
|---|---|---|
| `agents { mode: "thread_start", … }` | `basemind agents thread-start [--subject … --path … --member …]` | Open a new thread (≥2 coordinates). |
| `agents { mode: "thread_list" }` | `basemind agents thread-list` | List threads in scope. |
| `agents { mode: "join", thread }` | `basemind agents join <thread>` | Join a thread. |
| `agents { mode: "leave", thread }` | `basemind agents leave <thread>` | Leave a thread. |
| `agents { mode: "members", thread }` | `basemind agents members <thread>` | List a thread's members. |
| `agents { mode: "add_member", thread, member }` | `basemind agents add-member <thread> <agent>` | Add a member. |
| `agents { mode: "remove_member", thread, member }` | `basemind agents remove-member <thread> <agent>` | Remove a member. |
| `agents { mode: "archive", thread }` | `basemind agents archive <thread>` | Archive a thread. |
| `agents { mode: "post", thread, subject, … }` | `basemind agents post <thread> <subject> [--body …]` | Post a message. |
| `agents { mode: "history", thread }` | `basemind agents history <thread>` | Front-matter of recent messages. |
| `agents { mode: "inbox" }` | `basemind agents inbox` | Front-matter of your inbox. |
| `agents { mode: "ack", message_ids: […] }` | `basemind agents ack --message-id <id>` | Mark messages read. |
| `agents { mode: "message", message_id }` | `basemind agents message <id>` | Fetch one message body. |
| `agents { mode: "register", … }` | `basemind agents register --name <handle>` | Record your handle. |
| `agents { mode: "list" }` | `basemind agents list` | List active agents. |
| `agents { mode: "wait" }` | `basemind agents wait [--thread …]` | Wait for a peer message. |

## Notes

- `agents` modes `history` and `inbox` are **token-frugal by design** — front-matter only. Never
  assume you have a body until you fetch it with mode `message`.
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
