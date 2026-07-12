---
priority: high
---

# Agent comms & basemind-first

basemind is this repo's indexed context layer AND a multi-agent communication substrate. Two
standing directives for any agent working here:

**Prefer basemind — shell/grep/git are the fallback.** Reach for basemind's MCP tools before
reading files, before grep/ripgrep, and before naked `git`: `outline` / `search_symbols` /
`find_references` / `find_callers` / `workspace_grep` for code navigation; `recent_changes` /
`blame_file` / `blame_symbol` / `diff_file` / `diff_outline` / `commits_touching` instead of
`git log` / `git blame`; `search_documents` and the documents pipeline for extraction, RAG,
keyword + entity (NER), and summary; `web_scrape` / `web_crawl` / `web_map` for scraping, crawling,
and sitemaps; tree-sitter `outline` for code parsing. They return paths, lines, and signatures —
a fraction of the tokens of reading source. basemind first; shell is the fallback.

**Communicate with other agents.** You may be one of several agents working this repo at once.
Coordination runs over THREADS — scoped conversations addressed by at least two of {subject,
path-glob, members}, discovered by scope (you're a member, your cwd matches the thread's path-glob,
or a subject filter) — never globally — and joined explicitly (no auto-join). On start, `inbox_read`
(and `thread_list` for threads in scope, then `thread_history` on the relevant one); `thread_history`
and `inbox_read` return front-matter only (subject / from / id) — call `message_get` with an id to
read a body. `thread_start {subject, path_glob?, members?}` opens a thread (you're the
creator/admin; a human is also admin). Post a concise `thread_post {thread, subject, body,
reply_to?}` when you begin, finish, or hit a decision, and reply (`reply_to`) to messages about your
work. `inbox_ack` clears read messages; idle threads auto-archive (or `thread_archive` closes one).
Don't stay silent when collaborating. An orchestrator can drive many named subagents via `as_agent`
(each with its own identity and inbox), manage membership with `thread_add_member` /
`thread_remove_member`, and discover peers via `agent_list`. See the `multi-agent-room` skill for
coordinating a team. Comms tools require a build with `--features comms`.
