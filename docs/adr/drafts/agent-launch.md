---
status: proposed
date: 2026-11-16
---

# ADR draft: Cross-harness agent launch

- **Deciders:** basemind maintainers
- **Related:** ADR-0021 (agent comms v2, which left two constraints for this ADR), ADR-0018 and
  ADR-0020 (verb tools), ADR-0023 (crate layering), ADR-0024 (daemon and process model)

## Context

Agents in one repository already talk through basemind's comms (ADR-0021), but only agents a human
started. A host agent cannot start a peer in another harness: Claude Code cannot hand a review to
Codex, nor Codex a second opinion to Cursor. Each harness has its own way to be driven:

- Claude Code reads and writes `stream-json` on stdio and asks for permissions over a control
  channel.
- Codex runs `codex app-server`, a JSON-RPC dialect of its own.
- Cursor, Gemini CLI, Grok Build, Hermes Agent and OpenCode speak the Agent Client Protocol (ACP).

[monocode](https://github.com/hardbeat920/monocode) (MIT) drives all of them from one synchronous
Rust layer, `monocode-core`, written for a desktop application. The `agent-client-protocol` crate
(Apache-2.0, 5.3.0) is the maintained ACP SDK; it is runtime-agnostic and does not need tokio.

Starting a process that can edit files and run commands, on an agent's request, is the riskiest
thing basemind would do. ADR-0021 also left open what a launched agent's identity is and whether an
id that fails the grammar stops the launch.

## Decision

1. **A new library crate, `crates/basemind-agents`,** holds the provider layer: a `LaunchSpec`
   (harness, root, brief, identity, environment additions, MCP server, limits), a `Registry` of
   adapters, and a `Session` that yields typed events (text, tool call, approval request, finished
   with a status, error) and takes control (approve, deny, follow-up, stop). It is synchronous:
   threads and channels, errors as a `thiserror` enum. It depends on none of basemind's crates, so
   in ADR-0023's layering it sits beside `basemind-core` and is owned by `basemind-engine`.
2. **ACP goes through the `agent-client-protocol` crate.** The crate owns framing, request ids and
   the typed messages. `basemind-agents` owns the process, and runs the connection on
   `futures::executor::block_on` inside the session's thread. No async type is public.
3. **Lifted from monocode, because no maintained crate exists:** the Claude Code `stream-json`
   state machine, the Codex `app-server` state machine, process-group handling (spawn as a group
   leader, retire without cutting off a credential write, terminate the group) and the event and
   approval types, and the per-agent ACP dialect (Grok's arguments, the `yoloMode` `_meta`, the
   Hermes and OpenCode mode names). Each derived file names its source and commit;
   `ATTRIBUTIONS.md` records what was dropped and changed. monocode's hand-written ACP client is
   not taken.
4. **Identity bridge.** The launcher exports `BASEMIND_AGENT_ID` (the principal) and
   `BASEMIND_HOST` (the harness) to the launched agent, and injects `basemind serve` as an MCP
   stdio server in the harness's own form: Claude Code `--mcp-config` under `--strict-mcp-config`,
   Codex `-c mcp_servers.<name>.*`, ACP `mcpServers` in `session/new`. No tool of it is
   pre-approved unless the spec names it, and only Claude Code can pre-approve single tools. `BASEMIND_DATA_HOME` and
   `BASEMIND_COMMS_DIR` pass through, so both sides reach the same daemon.
   **An id that fails the grammar fails the launch.** ADR-0021 rule 1 would fall through to a
   derived id, and the agent would then post as someone its launcher did not announce.
5. **Safety model.** The crate enforces the first group against a hostile spec and a hostile
   child; the second belongs to the engine and the configuration, which do not exist yet.

   Enforced by `basemind-agents`:
   - **Approvals are denied by default.** A request is allowed only by a policy the caller
     supplies. It carries its kind and the raw request, and its text is rendered from the grant
     (command, file changes, permissions, tool input), never from the agent's reason alone. An ACP
     allow selects `allow_once` only. At most 64 requests wait for the caller.
   - **No permission bypass** (Claude `bypassPermissions`, Codex `danger-full-access`, ACP yolo
     modes) without an opt-in naming that harness and mode.
   - **Allow-listed arguments.** Caller-supplied arguments are limited to model and effort
     selection per harness; anything else, positionals included, fails the launch, opt-in or not.
   - **No project configuration.** Claude Code runs with no setting sources, hooks disabled and
     only the injected MCP server. Codex, Gemini, Cursor and OpenCode are not launched over a
     project file of theirs in the working directory or above it.
   - **Working directory.** It resolves inside the given root, checked at the plan and again in
     the child. The agent is not sandboxed to the root.
   - **Environment.** An allow-list plus additions that must be provider credentials or
     endpoints, so the launcher's credentials do not leak and an addition cannot redirect
     configuration, code loading or the basemind daemon.
   - **Limits.** Wall time and turns per session, enforced by killing the process group; a stop
     gets its grace. Frames, events, channels and tables are bounded, and dropping a session
     does not block.

   Left to the engine and the configuration:
   - **Off by default.** Launching is disabled until `[agents.launch] enabled = true`.
   - **Allow-list of harnesses** in the same table; a harness not named there cannot be started.
     The crate starts whatever executable the spec names.
   - **Depth, fan-out and budget**: a launched agent launching another, live sessions per
     principal, tokens or cost.
   - **Audit log.** Every launch, approval decision, limit expiry and stop is appended with the
     launcher's principal, the launched principal and the exact command (`CommandPlan`).
   - **Stop.** A launcher, and a human through the CLI, can stop any session it started.

### Tool surface, later

Two deferred verb tools beside `agent_post`, `agent_inbox` and `agent_threads`:

- `agent_launch { harness, brief, as?, thread?, working_dir?, max_turns?, wall_time_s? }` starts a
  session, joins the launched agent to `thread`, and returns its id and principal. It does not
  stream: output reaches the launcher as comms messages and through `agent_inbox`.
- `agent_stop { session }` stops a session the caller started.

Neither exists yet. The daemon will own the sessions (ADR-0024), because a session outlives the MCP
call that started it.

## Consequences

- A host agent can delegate to another harness and coordinate with it over the existing threads.
- New dependencies: `agent-client-protocol` and its schema crate (Apache-2.0), and `futures` with
  its executor. No tokio, no rmcp, no clap in the new crate.
- basemind now carries two protocol adapters that track upstream CLIs with no stability promise.
  They will break when Claude Code or Codex change their wire format. The recorded-transcript tests
  show what the adapters expect, not what the CLIs currently send.
- Flags for Gemini CLI and OpenCode, and the session-mode names for Gemini and Hermes, were taken
  from documentation and monocode's nearest equivalent, not verified against a live agent.
- A follow-up starts a new turn; mid-turn steering, which monocode supports, is not offered.
- Launching is Unix-only: the crate refuses to start a harness on Windows until its process tree
  is contained by a Job Object, which is not implemented.
- A descendant that leaves the process group (a helper that daemonizes) survives the session. The
  session still ends on time and stops reading its pipes; containing such a process needs a cgroup
  or a job object.
- Claude Code runs with an empty `--setting-sources`, which drops the user's own settings too, and
  Codex, Gemini, Cursor and OpenCode refuse to launch in a repository carrying their project
  configuration (this repository has a `.codex/`). The empty source list was not run against a
  live CLI; if a version rejects it, the launch fails rather than loading project rules.
- The project files of Grok, Hermes and a generic ACP agent are not known, so they are not
  checked.
- The environment allow-list means a harness that reads an API key from the environment gets it
  only when the configuration names it. Harnesses that sign in through a keychain or a config file
  in `HOME` work unchanged.

## Alternatives considered

- **Vendor monocode's provider layer whole** — rejected. `provider.rs` is one state machine for
  eleven harnesses, tied to the application's settings, accounts, usage tracking, warm process pool
  and transcript model. Most of it has no use here, and its ACP client duplicates a maintained
  crate. Its full-access mode auto-approves, which is the opposite of the default wanted here.
- **Shell out with no structure** (`claude -p`, `codex exec`) — rejected. There is no approval
  channel, so the only choices are a harness that cannot act or one with its permissions off, and
  no event stream to audit or to stop on.
- **ACP only** — rejected for now. Claude Code and Codex, the two most used harnesses, do not speak
  ACP natively; adapters that wrap them exist but add a Node process and a second trust boundary.
  Worth revisiting if both ship native ACP.
- **Launch through `shell spawn`** (the terminal sessions basemind already hosts) — rejected. A
  terminal carries no typed events and no approval requests, and scraping a TUI is brittle.

## Open questions for the owner

1. **Who may launch?** Recommendation: only a principal the configuration names, and never a
   launched agent by default (depth limit 1), so one prompt injection cannot fan out.
2. **Is the launched agent a sub-identity or a principal?** The grammar has no parent relation.
   Recommendation: a principal of its own, `<harness>-<repo>-<hash>-<n>`, with the launcher
   recorded in the audit log and the thread membership; `as` sub-names stay within one session.
3. **Should a human approve each launch?** Recommendation: no for harnesses on the allow-list in
   supervised mode, yes (a CLI confirmation) for any launch that carries a bypass opt-in.
4. **Budget.** The harnesses report token usage in three different shapes and the adapters
   currently drop it. Recommendation: ship wall time, turns, depth and fan-out first; add a token
   budget when the events carry usage.
5. **Pre-approving basemind's own tools.** Nothing is pre-approved by default, so every comms call
   is a request the default policy denies. The spec can name single tools, which Claude Code
   pre-approves; Codex and ACP agents cannot, and refuse a non-empty list. Recommendation: the
   engine names the `agent_*` tools once they are final, and its policy approves them by
   `ApprovalKind::McpTool` and tool name for the other harnesses.
6. **Windows.** The crate refuses to launch there. Recommendation: keep `agent_launch` Unix-only
   until a Job Object contains the tree and has a test.
