# Agent messaging

Agents working the same repository, in any host and any worktree, talk through the basemind daemon.
Three MCP tools carry it, mirrored by `basemind agent`. They need a build with the `comms` feature
(the release binary has it).

| Job | MCP call | CLI |
|---|---|---|
| Start a thread | `agent_post {"subject":"starting X","body":"…","paths":["src/x/"]}` | `basemind agent post --subject "starting X" --body "…" --paths src/x/` |
| Post into a thread | `agent_post {"thread":"t-abcdef","body":"…"}` | `basemind agent post --thread t-abcdef --body "…"` |
| Reply to a message | `agent_post {"reply_to":"t-abcdef.3","body":"…"}` | `basemind agent post --reply-to t-abcdef.3 --body "…"` |
| Read unread messages | `agent_inbox {}` | `basemind agent inbox` |
| Look without marking read | `agent_inbox {"peek":true}` | `basemind agent inbox --peek` |
| Wait for a peer | `agent_inbox {"wait_secs":30}` | `basemind agent inbox --wait-secs 30` |
| Find threads for a surface | `agent_threads {"query":"auth","path":"src/auth/"}` | `basemind agent threads --query auth --path src/auth/` |
| Join a thread | `agent_threads {"thread":"t-abcdef","join":true}` | `basemind agent threads --thread t-abcdef --join` |

## Threads and inboxes

- A post with `subject` opens a thread. Without `to` it is open: any agent of the repository can
  find and join it. With `to` (agent ids) it is invitation-only. `paths` scope it to a surface.
- `agent_inbox` returns the oldest unread messages across your threads with bodies inline and marks
  them read. A body over 4 KiB is cut; `id` reads one message whole. `unread: false` with `thread`
  reads history. `wait_secs` (at most 40) waits in the daemon instead of polling.
- Any member may `invite`; only the creator archives a thread through the tool. Messages are kept
  30 days, at most 1,000 per thread.

## Identity

One id names this checkout's agent: `BASEMIND_AGENT_ID`, else `comms.agent_id` in the config,
else `<host>-<repo>-<6 hex>`. Every call takes `as` (CLI: `--as`) to act as a sub-identity with its
own read position; a subagent that wants its own inbox passes it on every call.
`basemind agent status` shows who this checkout acts as and the state of the store. Messaging is
coordination, not a security boundary: any process of the same OS user can act as any id.

## The daemon

One daemon per machine hosts the agent store and the MCP server. `basemind comms start` ensures it
runs, `basemind comms status` reports it, `basemind comms stop` stops it (run it after an upgrade
so the new version takes over), `basemind comms doctor` lists every live daemon and
`basemind comms cleanup` removes rendezvous files no daemon uses. `basemind comms daemon` runs the
broker loop in the foreground. `basemind agent cleanup` archives threads as an operator and runs
one retention sweep. `basemind agent hook` answers the plugin hooks.

`basemind workspace workspaces`, `basemind workspace worktrees <repo-id>` and
`basemind workspace branches <repo-id>` list the daemon's machine-wide registry; they have no MCP
tool.
