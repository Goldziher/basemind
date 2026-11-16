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

1. **A new library crate, `crates/basemind-agents`,** holds the provider layer. A launch has two
   halves. The `LaunchConfig` is trusted and comes from the operator's configuration: the harness
   and its executable, the root, the identity, environment values, the MCP server, the permission
   mode, the opt-ins, the approval policy and the caps on wall time and turns. The `LaunchRequest`
   is what a model may choose: the brief (at most 1 MiB), a working directory relative to the root,
   a model and an effort, and a wall time and turn count that are clamped to the caps. The types
   carry the split: a request cannot express an executable, an environment value or an opt-in. A
   `Registry` of adapters turns the pair into a `Session` that yields typed events (text, tool
   call, approval request, finished with a status, error) and takes control (approve, deny,
   follow-up, stop). It is synchronous: threads and channels, errors as a `thiserror` enum. It
   depends on none of basemind's crates, so in ADR-0023's layering it sits beside `basemind-core`
   and is owned by `basemind-engine`.
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
   pre-approved unless the config names it, and only Claude Code can pre-approve single tools.
   `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR` pass through, so both sides reach the same daemon.
   **An id that fails the grammar fails the launch.** ADR-0021 rule 1 would fall through to a
   derived id, and the agent would then post as someone its launcher did not announce. The id is
   self-asserted: see Consequences.
5. **Safety model.** The crate enforces the first group against hostile model-chosen fields (the
   request) and a hostile child. It trusts the config. The second group belongs to the engine and
   the configuration, which do not exist yet.

   Enforced by `basemind-agents`:
   - **Approvals are denied by default.** A request is allowed only by a policy the config
     supplies. It carries its kind and the raw request, and its text is rendered from the grant
     (command, file changes, permissions, tool input), never from the agent's reason alone. A Codex
     file change carries the item's full changes; one whose changes were not announced or are over
     1 MiB is denied without asking. An ACP allow selects `allow_once` only. At most 16 requests
     wait for the caller. Approval ids are the crate's own, so a late answer cannot reach a newer
     request that reuses a wire id. A Codex or ACP request still pending when its turn ends, and
     any request pending when the session stops, is answered with a denial (Claude Code's requests
     of a finished turn are only reported withdrawn: the harness no longer waits on them).
   - **No permission bypass** (Claude `bypassPermissions`, Codex `danger-full-access`, ACP yolo
     modes) without a `BypassOptIn` naming that harness and mode. Like every opt-in it is config
     data: the crate checks that it matches, not who set it.
   - **No mode whose behaviour is unverified, without consent.** Claude Code's and Codex's adapters
     set the permission mode explicitly. An ACP agent in any mode but an authorized bypass needs an
     `UnverifiedModeOptIn` naming the harness and the mode, because what it does unasked was not
     verified. OpenCode is never launched supervised: its default edits files and runs commands
     without asking (from OpenCode's documentation, not run here).
   - **Only a model and an effort reach the command line from a request,** as the harness's own
     flags after a grammar check; a harness that takes neither at launch refuses them.
   - **No project configuration.** Claude Code is asked for no setting sources, hooks disabled and
     only the injected MCP server, and the session fails unless its `system/init` frame reports the
     requested permission mode and exactly the injected MCP server. Codex, Gemini, Cursor and
     OpenCode are not launched over a project file of theirs in the working directory or above it
     (for Cursor, anything in `.cursor/` but `rules`); a path that cannot be read counts as
     present.
   - **Executables.** The harness executable and the MCP server's command are resolved to absolute
     paths at the plan, against the absolute entries of the launcher's `PATH`, and refused when
     they lie inside the root, directly or through a link. The child's `PATH` keeps only absolute
     entries.
   - **Working directory.** It resolves inside the root, checked at the plan and again in the
     child. The agent is not sandboxed to the root.
   - **Environment.** An allow-list plus config additions whose names must be provider credentials
     or a few provider settings, so the launcher's credentials do not leak and an addition cannot
     redirect configuration, code loading, executable search or the basemind daemon.
   - **Limits.** Wall time and turns per session, enforced by killing the process group; a stop
     gets its grace. Frames, events, channels and tables are bounded. An ACP agent that stops
     reading while the client has 16 lines queued for it fails the session. Dropping a session
     waits at most 2 s for its supervisor; the group is killed either way.

   Left to the engine and the configuration:
   - **Off by default.** Launching is disabled until `[agents.launch] enabled = true`.
   - **Allow-list of harnesses and executables** in the same table; a harness not named there
     cannot be started. The crate starts whatever executable the config names, outside the root.
   - **Who launches.** Only config-named principals; a session the daemon spawned may not launch.
   - **Depth, fan-out, concurrency and budget**: a launched agent launching another, live sessions
     per principal and per daemon, tokens or cost.
   - **Audit log.** Every launch, approval decision, limit expiry and stop is appended from the
     daemon's own spawn record with the launcher's principal, the launched principal and the exact
     command (`CommandPlan`).
   - **The injected server's surface.** `basemind serve` exposes every tool to the launched agent.
     The wiring has to start it with a restricted profile (the comms verb tools only); no domain
     tool is pre-approved by name.
   - **Stop.** A launcher, and a human through the CLI, can stop any session it started.

### Tool surface, later

Two deferred verb tools beside `agent_post`, `agent_inbox` and `agent_threads`:

- `agent_launch { harness, brief, as?, thread?, working_dir?, max_turns?, wall_time_s? }` starts a
  session, joins the launched agent to `thread`, and returns its id and principal. Its fields map
  to a `LaunchRequest`; `harness` selects a `LaunchConfig` the configuration defines. It does not
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
- Nothing here was run against a live harness. Flags for Gemini CLI and OpenCode, the session-mode
  names for Gemini and Hermes, the fields of Claude Code's `system/init` frame (`permissionMode`,
  `mcp_servers`) and the effect of an empty `--setting-sources` were taken from documentation and
  monocode. Where a guess would weaken a check, the check fails closed: a Claude Code version
  whose init frame lacks those fields, or lists another MCP server (an account connector, say),
  fails every launch.
- **User-level configuration still applies** to every harness but Claude Code: the child inherits
  `HOME` and `XDG_*`, so `~/.codex/config.toml`, `~/.gemini/settings.json`,
  `~/.cursor/cli-config.json`, OpenCode's global `permission` and Hermes's settings can approve
  tools, add MCP servers or run hooks behind the policy. A launcher-owned config home per harness
  would close this; it is not done because none of those homes was verified to keep sign-in
  working. Claude Code still reads managed (policy) settings, `CLAUDE.md` memory files and
  `~/.claude.json`.
- **Default modes that do not ask.** ACP agents run in modes nobody here verified, under an
  explicit opt-in; OpenCode's default is refused. Codex `ReadOnly` uses approval policy `never`
  with a read-only sandbox: nothing is asked, and whatever the sandbox blocks fails.
- **PATH and executables.** The harness's own lookups (`#!/usr/bin/env node`, helper binaries)
  search the child's `PATH`, which keeps the launcher's absolute entries; a directory on it that
  the agent can write is outside what the crate checks.
- **Account routing.** The config's credential and the provider settings it may add
  (`AWS_REGION`, `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`, ...) choose which account the
  harness bills and where its requests go; they are trusted config values, never request fields.
  An environment addition cannot name an endpoint (`*_BASE_URL`); a deployment that needs one sets
  it in the harness's own user configuration.
- **An ACP grant is the agent's own description.** The approval shows the tool call the agent
  described in `session/request_permission`; nothing ties that description to what the agent then
  runs. ACP requests never carry `ApprovalKind::McpTool`, since ACP does not say which tool calls
  are MCP calls.
- **`BASEMIND_AGENT_ID` is self-asserted.** A launched agent with a shell can export any id, or
  read its launcher's, and post as it. Depth limits and audit cannot rest on the env identity; the
  daemon has to tag the sessions it spawned.
- **Project files race the check.** The project-file check is a snapshot at plan time: a file
  written between the plan and the harness reading its configuration, or during the session, is
  only seen by the next launch, which it then refuses. This repository has a `.codex/`, so Codex
  refuses to launch in it.
- **Per-session memory.** Bounded, but not small: two queued frames and the one being read (each
  up to 16 MiB, more once parsed), 16 MiB of queued events, 16 pending requests of up to 1 MiB, 16
  Codex file changes of up to 1 MiB, 64 tracked tool calls of up to about 192 KiB and 8 MiB of text
  de-duplication: on the order of 150 MiB in the worst case. The engine has to cap concurrent
  sessions per daemon.
- **Host preconditions.** The launcher must not ignore SIGCHLD or set `SA_NOCLDWAIT` (a launch is
  refused then), and must not reap children it did not start (`waitpid(-1)` elsewhere in the
  process): a leader reaped behind the session's back is never signalled again, and its group is
  left alone. SIGPIPE must be ignored, as Rust binaries do by default, or a write to an exited
  harness kills the launcher.
- Launching is Unix-only: the crate refuses to start a harness on Windows until its process tree
  is contained by a Job Object, which is not implemented.
- A descendant that leaves the process group (a helper that daemonizes) survives the session. The
  session still ends on time and stops reading its pipes; containing such a process needs a cgroup
  or a job object.
- A follow-up starts a new turn; mid-turn steering, which monocode supports, is not offered.
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
- **One spec type with every field** — rejected. A spec a model fills in could name the
  executable, the MCP command, the environment values and the opt-ins; the split makes the
  model-facing part unable to express them.

## Open questions for the owner

The proposed answers are the security review's recommendations; the owner decides.

1. **Who may launch?** Proposed: only principals the configuration names, and launching is off by
   default. The daemon tags the sessions it spawned and refuses `agent_launch` from them, so depth
   is limited to 1 without resting on the self-asserted env identity.
2. **Is the launched agent a sub-identity or a principal?** The grammar has no parent relation.
   Proposed: a principal of its own (`<harness>-<repo>-<hash>-<n>`), with the launcher recorded
   in an audit entry written from the daemon's own spawn record, not from anything the agent
   reports; `as` sub-names stay within one session.
3. **Should a human approve each launch?** Proposed: yes for any launch with a `BypassOptIn`, for
   any harness whose default mode is not verified to ask (every launch with an
   `UnverifiedModeOptIn`), and for limits above the configuration's defaults; no for Claude Code
   and Codex in supervised mode within the defaults.
4. **Budget.** The harnesses report token usage in three different shapes and the adapters
   currently drop it. Proposed: ship wall time, turns, depth and fan-out first, clamp every request
   to the config's caps (the crate already does for wall time and turns), cap concurrent sessions
   per daemon, and add a token budget when the events carry usage.
5. **Pre-approving basemind's own tools.** Nothing is pre-approved by default, so every comms call
   is a request the default policy denies. Proposed: no pre-approval of domain tools by name; the
   injected server runs a restricted serve profile with the comms verb tools only, and the policy
   decides on those.
6. **Windows.** The crate refuses to launch there. Proposed: keep `agent_launch` refused on Windows
   until a Job Object contains the tree and has a test.
