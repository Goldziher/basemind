---
priority: high
---

# Agent comms & basemind-first

basemind is this repo's indexed context layer AND a multi-agent communication substrate. Two
standing directives for any agent working here:

**Prefer basemind — shell/grep/git are the fallback.** Reach for basemind's MCP tools before
reading files, before grep/ripgrep, and before naked `git`: `code` modes `outline` / `symbols` /
`references` / `callers` / `grep` for code navigation; `git` modes `recent` / `blame` /
`blame_symbol` / `diff` / `diff_outline` / `touching` instead of `git log` / `git blame`; `memory`
mode `documents` for extraction, RAG, keyword + entity (NER), and summary; `web` modes `scrape` /
`crawl` / `map` for scraping, crawling, and sitemaps; `code` mode `outline` (tree-sitter) for code
parsing. They return paths, lines, and signatures — a fraction of the tokens of reading source.
basemind first; shell is the fallback.

**Communicate with other agents.** You may be one of several agents working this repo at once.
Coordination runs over THREADS — scoped conversations addressed by at least two of {subject,
path-glob, members}, discovered by scope (you're a member, your cwd matches the thread's path-glob,
or a subject filter) — never globally — and joined explicitly (no auto-join). All of this is the
`agents` MCP tool (`--features comms`), dispatched by `mode`. On start, `agents inbox` (and `agents
thread_list` for threads in scope, then `agents history` on the relevant one); `history` and `inbox`
return front-matter only (subject / from / id) — call `agents message` with an id to read a body.
`agents thread_start {subject, path_glob?, members?}` opens a thread (you're the creator/admin; a
human is also admin). Post a concise `agents post {thread, subject, body, reply_to?}` when you begin,
finish, or hit a decision, and reply (`reply_to`) to messages about your work. `agents ack` clears
read messages; idle threads auto-archive (or `agents archive` closes one). Don't stay silent when
collaborating. An orchestrator can drive many named subagents via `as_agent` (each with its own
identity and inbox), manage membership with `agents add_member` / `agents remove_member`, and
discover peers via `agents list`. See the `multi-agent-room` skill for coordinating a team.
