---
type: Decision
title: Agent Comms
x-ai-rulez:
  kind: rule
  id: agent-comms
  metadata:
    priority: high
---

# Agent comms & basemind-first

basemind is this repo's indexed context layer AND a multi-agent communication substrate. Two
standing directives for any agent working here:

**Prefer basemind — shell/grep/git are the fallback.** Reach for basemind's MCP tools before
reading files, before grep/ripgrep, and before naked `git`: `code_outline` / `code_symbols` /
`code_refs` / `code_grep` / `code_read` for code navigation; `git_log` / `git_blame` / `git_diff`
instead of `git log` / `git blame` / `git diff`; `docs_search` for document search with keyword +
entity (NER) filters and summaries; `find_files` to locate a file. The tools are always loaded and
take exact arguments; they return paths, 1-based lines, and signatures — a fraction of the tokens
of reading source. basemind first; shell is the fallback.

**Communicate with other agents.** You may be one of several agents working this repo at once.
Messaging is opt-in and secondary to code navigation: three deferred MCP tools (`--features comms`),
found by tool search, not loaded at session start. `agent_post {body, subject?, thread?, reply_to?,
to?, paths?}` sends: `subject` starts a thread, `thread` or `reply_to` continues one, and `to` makes a
new thread invitation-only (without it the thread is open to the repository). `agent_inbox {}` reads
unread messages with bodies inline and marks them read; `peek: true` does not, `unread: false` reads
history, `id` reads one message whole, `wait_secs` (0 to 40) waits in the daemon. `agent_threads`
lists threads (`query`, `path`), describes one (`thread`), and changes it with `join`, `leave`,
`invite` or `archive`. Post a concise note when you begin, finish, or hit a decision, and reply
(`reply_to`) to messages about your work. Every tool takes an optional `as` (`[a-z0-9_-]{1,32}`) for
a sub-identity: a subagent that wants its own inbox passes it. The CLI mirrors the tools
(`basemind agent post|inbox|threads`) and adds `basemind agent status` and `basemind agent cleanup`.
Spec: `docs/comms-v2.md`. See the `basemind-multi-agent-room` skill for coordinating a team.
