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

**Communicate with other agents.** You may be one of several agents working this repo at once. On
start, check `room_list` + `inbox_read` (and recent `room_history`) for what's been said;
`room_history` and `inbox_read` return front-matter only (subject / from / id) — call `message_get`
with an id to read a body. Post a concise `room_post {room, subject, body, reply_to?}` when you
begin, finish, or hit a decision, and reply (`reply_to`) to messages about your work. Don't stay
silent when collaborating. An orchestrator can drive many named subagents via `as_agent` (each
with its own identity and inbox), send direct messages with `dm_send`, and discover peers via
`agent_list`. See the `multi-agent-room` skill for coordinating a team. Comms tools require a
build with `--features comms`.
