---
status: proposed
date: 2026-11-17
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
   `BASEMIND_HOST` (the harness) to the launched agent, and, when the `Supervisor` launched it,
   `BASEMIND_AGENT_DEPTH` and `BASEMIND_LAUNCH_TOKEN` (decision 6), and injects `basemind serve` as an MCP
   stdio server in the harness's own form: Claude Code `--mcp-config` under `--strict-mcp-config`,
   Codex `-c mcp_servers.<name>.*`, ACP `mcpServers` in `session/new`. No tool of it is
   pre-approved unless the config names it, and only Claude Code can pre-approve single tools.
   `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR` pass through, so both sides reach the same daemon.
   **An id that fails the grammar fails the launch.** ADR-0021 rule 1 would fall through to a
   derived id, and the agent would then post as someone its launcher did not announce. The id is
   self-asserted: see Consequences.
5. **Safety model.** The crate enforces the first group against hostile model-chosen fields (the
   request) and a hostile child. It trusts the config. The second group, the launch policy, is
   also in the crate, in its `Supervisor` (decision 6). The third belongs to the engine and the
   configuration, which do not exist yet.

   Enforced by `basemind-agents`:
   - **Approvals are denied by default.** A request is allowed only by a policy the config
     supplies. It carries its kind and the raw request, and its text is rendered from the grant
     (command, file changes, permissions, tool input), never from the agent's reason alone. A Codex
     file change carries the item's full changes; one whose changes were not announced or are over
     1 MiB is denied without asking. A Codex MCP tool call, which Codex 0.169 asks for as an
     `mcpServer/elicitation/request` of kind `mcp_tool_call`, goes to the policy; an allow
     accepts that one call. A Codex command that asks for more than its sandbox
     (`additionalPermissions`, `networkApprovalContext` in Codex 0.169's generated schema) is a
     permissions request, and its detail shows that access: an accept grants it with the command. An ACP allow selects `allow_once` only. At most 16 requests wait for
     the caller. Approval ids are the crate's own, so a late answer cannot reach a newer request
     that reuses a wire id, and two pending requests sharing a wire id are both denied. A Codex or
     ACP request still pending when its turn ends, and any request pending when the session
     stops, is answered with a denial and reported withdrawn; one still pending when the session
     ends any other way (a failed or exited harness) is reported withdrawn before `Finished` (Claude Code's requests of a finished
     turn are only reported withdrawn: the harness no longer waits on them).
   - **No permission bypass** (Claude `bypassPermissions`, Codex `danger-full-access`, ACP yolo
     modes) without a `BypassOptIn` naming that harness and mode. Like every opt-in it is config
     data: the crate checks that it matches, not who set it.
   - **No mode whose behaviour is unverified, without consent.** Claude Code's and Codex's adapters
     set the permission mode explicitly; Codex refuses `AcceptEdits`, because its nearest mode
     (`on-request`, workspace-write) ran a shell command unasked on Codex 0.169. An ACP agent in
     any mode but an authorized bypass needs an `UnverifiedModeOptIn` naming the harness and the
     mode, because what it does unasked was not verified, except OpenCode supervised: OpenCode
     2.3.0's default wrote a file unasked, but with `OPENCODE_CONFIG_CONTENT={"permission":"ask"}`,
     which every OpenCode launch now sets, it asked before an edit, a shell command and an MCP
     call. An ACP agent that reports switching itself to another session mode fails the session,
     whether it reports a mode or, as OpenCode 2.3.0 does, a mode-category config option; no live
     OpenCode turn sent such an update, so that shape is the protocol's.
   - **Only a model and an effort reach the command line from a request,** as the harness's own
     flags after a grammar check; a harness that takes neither at launch refuses them. A brief or
     follow-up may not start with `/`: Claude Code 2.1.325 ran a prompt of `/context` as its own
     slash command. Errors about a request's fields repeat neither the root, a path nor a whole
     value, and a working directory that does not exist reads the same as one outside the root,
     so an error does not tell a model whether a path outside the root exists.
     A failed session's error is split: `Event::Diagnostics` carries the full text and the
     harness's stderr tail for the launcher's logs, `Event::Error` one sanitised line of at most
     512 bytes without the stderr, which is all the engine relays to a model.
   - **No work outside the session.** Claude Code is launched with `--disallowedTools` for the
     tools that start or schedule work the session's limits and process group do not reach
     (remote triggers, cron jobs, wake-ups, messages to other projects, push notifications,
     workflows, other worktrees), and its init frame may offer none of them; Claude Code 2.1.326
     dropped all of them under the flag.
   - **No project configuration.** Claude Code is asked for no setting sources, hooks disabled and
     only the injected MCP server. The brief is held until its `initialize` answer reports the
     requested permission mode, only built-in commands and no agent a `.claude/` directory of the
     working directory, its ancestors or `HOME` defines. `system/init` arrives only after the
     brief, so its check ends a session that may already have begun work: it has to report the
     requested permission mode, exactly the injected MCP server as loaded from `--mcp-config`
     (`source: "dynamic"`), only built-in plugins and no plugin or `.claude/`-defined skill or
     agent; every later init frame is checked again, so a mode changed mid-session fails it. A
     project's `.claude/` and `.mcp.json` do not refuse the launch: they were verified not to load
     under these flags (2.1.325, 2.1.326), and refusing them would refuse most repositories. Verified on Claude Code 2.1.325: under these
     flags a project `CLAUDE.md`, `.claude/` hooks, permission rules, agents, commands and skills,
     `.mcp.json`, user plugins and claude.ai connectors were not loaded. Codex, Gemini, Cursor and
     OpenCode are not launched over a project file of theirs in the working directory or above it
     (for Cursor, anything in `.cursor/` but `rules`); a path that cannot be read counts as
     present. Gemini CLI, which reads the first `.env` above its working directory, and the ACP
     agents whose loaders are unknown (Cursor, Grok, Hermes, generic) are not launched over a
     `.env*` file there either: `GOOGLE_GEMINI_BASE_URL` or `HTTPS_PROXY` in it would send their
     credentials elsewhere. Claude Code 2.1.326, Codex 0.169 and OpenCode 2.3.0 were seen not to
     read one, nor a `bunfig.toml`; Gemini's lookup is from its documentation and not run.
   - **The repository's git configuration.** A harness runs git on the repository by itself,
     outside the sandbox it gives the agent: Codex 0.169 ran `git status` when a turn started, and
     a clean filter in `.git/config` ran with it. Every child gets `core.fsmonitor=false` through
     `GIT_CONFIG_COUNT` (Claude Code, Codex and OpenCode already pass it on their own calls), and a
     repository whose `config` or `config.worktree` sets a filter, a diff driver, an external diff
     or an include is refused, Git LFS's local `filter.lfs` included. User and system git
     configuration are the user's own and are not read.
   - **Executables.** The harness executable and the MCP server's command are resolved to absolute
     paths at the plan, against the absolute entries of the launcher's `PATH`, and refused when
     they lie inside the root, directly, at the end of their links or through any link on the
     way. The check is not atomic with the exec. The child's `PATH` keeps only absolute entries
     outside the root.
   - **Working directory.** It resolves inside the root, checked at the plan and again in the
     child. The agent is not sandboxed to the root. The root itself may not contain the launcher's home or
     XDG configuration and data directories, nor be, contain or lie inside a harness's own
     directory or basemind's store and comms directories, where the harness's "inside the
     workspace" rules would let the agent rewrite them.
   - **Environment.** An allow-list plus config additions whose names must be provider credentials
     or a few provider settings, so the launcher's credentials do not leak and an addition cannot
     redirect configuration, code loading, executable search or the basemind daemon.
   - **Limits.** Wall time (on a clock that counts sleep: `CLOCK_BOOTTIME` on Linux,
     `CLOCK_MONOTONIC` on macOS) and turns per session, enforced by killing the process group; a stop,
     and a session that failed, get the stop grace. Frames (16 MiB and 256 Ki JSON values),
     events, channels and tables are bounded. An ACP agent that stops reading while the client has
     16 lines queued for it fails the session. Dropping a session waits at most 2 s for its
     supervisor; the group is killed either way. A launcher that dies outright still takes the group with it:
     a keeper process forked into the group between fork and exec waits on a pipe whose only write
     end the launcher holds, and kills its own group when that end closes, on Linux and macOS
     alike. `PR_SET_PDEATHSIG` was rejected: it fires when the spawning thread exits, which a
     launcher on a thread pool does routinely, and it signals only the leader. A descendant that
     left the group still survives; the engine may record the sessions' process group ids and
     check them at start, but nothing here requires it.

6. **Launch policy and supervisor.** Every launch goes through a `Supervisor`, which applies a
   `LaunchPolicy` before any adapter runs, owns the sessions and writes the audit log. The
   policy is configuration, validated when it is built (`PolicySpec` -> `LaunchPolicy::new`).
   - **Off by default.** `LaunchPolicy::default()` allows no flavor and no launcher, and every
     launch is refused (`disabled`). A supervisor refuses a `LaunchConfig` for a flavor its
     policy does not allow, two for one flavor, and one that widens what the flavor's rule
     grants: another permission mode, bypass or unverified-mode opt-in, an `ApprovalPolicy` the
     rule does not consent to (the crate cannot see what a policy decides, and one that allows
     everything is a bypass), a pre-approved MCP tool the rule does not name, or a stop grace
     over 60 s.
   - **Allow-list of flavors and their modes.** Each allowed flavor names its permission mode. A
     bypass mode needs a `BypassOptIn` naming that flavor and its own mode name; an opt-in on any
     other mode is refused rather than kept, and a flavor without a bypass mode (OpenCode,
     Cursor) cannot be given one.
   - **Who launches.** A launcher is a principal (ADR-0021 rule 1) the policy names, at depth 0;
     a principal starting with `launched-`, the prefix every launched agent gets, cannot be
     named. Every caller carries a `Peer`, the process the kernel reports for its connection
     (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS), bound to its start time; a caller
     cannot be built without one. The supervisor decides by that process, not by the claim.
     Each harness is started with `setsid`, so it leads a POSIX session of its own and nothing it
     starts can join a process group of another session (`setpgid` across sessions is `EPERM`).
     A peer in a live session (by session id, by ancestry read from `/proc` or `proc_pidinfo`,
     or as a detached process recorded for it) is that session whatever principal it claims, at
     the depth the supervisor recorded, and a token it presents has to be that session's own. A
     peer below the supervisor in no live session is nobody. Only a peer outside the
     supervisor's tree is the principal it names, and only it can be made the operator:
     `Supervisor::operator(peer)` is the only constructor, and the peer is placed again on every
     call. The operator stops and answers any session and launches none. The env identity is
     self-asserted (ADR-0021 rule 4) and is never consulted.
   - **Identity of a launched agent.** A principal of its own, `launched-<flavor>-<id>-<8 hex>`,
     exported in `BASEMIND_AGENT_ID` with the config's host, its depth in
     `BASEMIND_AGENT_DEPTH` and 256 bits of the operating system's randomness, as hex, in
     `BASEMIND_LAUNCH_TOKEN`. All three pass to the injected MCP server. No config can set a
     `BASEMIND_*` variable, so a config cannot forge them.
   - **Limits.** `max_depth` (at most 8), `max_concurrent` live sessions (at most 32, about 6 GiB
     of worst-case session memory), `max_children` live sessions per launcher, a wall time (at
     most 24 h) and an optional budget (`max_launches` over the supervisor's life, `max_turns`
     per session). A launch over any of them is refused with a typed `SupervisorError` naming
     the limit (`depth_exceeded`, `concurrency_exceeded`, `fan_out_exceeded`,
     `budget_exhausted`). A session's wall time is the least of the request's, the config's and
     the policy's. A slot is reserved before the harness starts, so concurrent launches cannot
     overshoot; an adapter refusal, a panic in the adapter, a launch record that cannot be
     written and a supervising thread that cannot be started each release it and spend no
     budget. A session that is stopping launches nothing.
   - **Audit log.** An `AuditSink` gets one record per launch (written before the harness
     starts), refusal (the policy's or the adapter's), stop, exit and approval decision (settled
     by the policy, by default, withdrawn, or answered by a caller through the supervisor, with
     who answered). A record carries the time, the caller and its depth, the session, the
     launched principal, the flavor, the root, the depth, the limits, the flavor's caveats and a
     fixed outcome code; never a brief, an approval's command or detail, an environment value, a
     token or harness text. Records are written after the supervisor's lock is released, so a
     slow disk does not stall the sessions. Refusals are coalesced per caller and outcome: 8 a
     minute are written, the rest counted into the next one (`suppressed`) or a final record
     when the supervisor is dropped. A failed write: a launch whose record cannot be written is
     refused; an allow answered through the supervisor whose record cannot be written is not
     delivered (the request stays pending and is denied when its turn or the session ends);
     every other record's failure is counted and its session runs on. The engine picks the file;
     `FileAuditSink` appends one JSON object per line to a file it creates with mode 0600, and
     refuses a symbolic link, a file with another hard link, a non-regular file (opened
     non-blocking, so a FIFO cannot hang it), another user's file or one others can read or
     write. It stops at a cap (64 MiB by default) with an `audit_full` marker and an
     `audit_dropped` count; past the cap a launch is refused. It is append-only as far as this
     writer goes, not tamper-resistant: any process of the same user can rewrite it.
   - **Sessions.** `status`, `list`, `wait`, `stop` (by the launcher or the operator; it reaches
     every session below the one stopped), `answer` for a pending approval (same), `stop_all`,
     `reap`. A thread per session reads its events, hands each to an optional
     `SessionObserver` and audits approvals and the exit; an observer that panics is dropped for
     that session and the session is stopped. Dropping the supervisor stops every session within
     its stop grace, kills what is left and returns.
   - **Per-flavor caveats**, reported on the flavor's policy, on each launch and status, and in
     the launch record. `DetachedServer` (OpenCode): while the session runs the supervisor reads
     the process table every 200 ms and records, by pid and start time, each process that
     descends from the harness or sits in a POSIX session the harness or a recorded descendant
     leads; it stops once the harness is gone. Once the session ends it waits up to 2 s, then
     kills what is left: a whole group only when every member is the session's and the group is
     neither init's nor the launcher's, else each process alone, each checked again right before
     the signal (through a pidfd on Linux). Nothing is killed without a listing, and the session
     is reported finished only then.
     `UserMcpServers` (OpenCode, Codex): the harness starts the user's own MCP servers and
     plugins from its user-level configuration, OpenCode even with an empty `mcpServers`;
     nothing in the launch switches that off, so an operator allowing the flavor accepts it.

   Left to the engine and the configuration, none of which the crate does, and all of which have
   to exist before `agent_launch` is offered:
   - **Configuration.** An `[agents.launch]` table that builds the `PolicySpec` and one
     `LaunchConfig` per allowed flavor (executable, root, env additions, approval policy,
     caps), with `enabled = false` mapping to `LaunchPolicy::default()`, and the audit log's
     path under basemind's data directory.
   - **One supervisor per daemon** (ADR-0024), since a session outlives the MCP call that
     started it.
   - **Caller identity on the wire.** The daemon has to build a `Caller` from the connection:
     the `Peer` from `Peer::of_socket` on the accepted connection (never a pid the client
     sends), the principal from `Hello` (ADR-0021 rule 3), and the launch token the injected
     `basemind serve` read from `BASEMIND_LAUNCH_TOKEN`. Carrying the token is the protocol bump
     ADR-0021 anticipated. A transport without peer credentials cannot reach the supervisor.
   - **The operator channel.** `basemind agent approve` / `deny` / `stop` reach the supervisor as
     `Supervisor::operator(peer)` over a channel the launched agents are not handed: not a tool
     of the injected server, and not the socket whose path their environment names. The crate
     refuses a peer inside or below its tree; the channel is what keeps other processes of the
     same user out. On Linux the daemon should make itself a child subreaper
     (`PR_SET_CHILD_SUBREAPER`) and reap those orphans by pid, so an agent's process that
     double-forks away still descends from it.
   - **The injected server's surface.** `basemind serve` exposes every tool to the launched
     agent. The wiring has to start it with a restricted profile (the comms verb tools and
     `agent_launch` / `agent_stop` only); no domain tool is pre-approved by name.
   - **Relaying approvals and output.** An `Ask` verdict waits for the launcher; the engine has
     to deliver the request (through the observer, as a comms message) and its answer
     (`Supervisor::answer`). A model-facing error is the `SupervisorError` code, not its text.

### Tool surface, later

Two deferred verb tools beside `agent_post`, `agent_inbox` and `agent_threads`:

- `agent_launch { harness, brief, as?, thread?, working_dir?, model?, effort?, max_turns?,
  wall_time_s? }` calls `Supervisor::launch` for the caller, joins the launched agent to
  `thread`, and returns the session id, the launched principal, its depth and the flavor's
  caveats. Its fields map to a `LaunchRequest`; `harness` selects the flavor, whose
  `LaunchConfig` the configuration defines. It does not stream: output reaches the launcher as
  comms messages and through `agent_inbox`.
- `agent_stop { session }` calls `Supervisor::stop`, which refuses a session the caller did not
  launch.

Neither exists yet. The CLI adds `basemind agent sessions` (`list`), `agent stop` and `agent
approve` / `deny` as the operator, over the operator channel above.

## Consequences

- A host agent can delegate to another harness and coordinate with it over the existing threads.
- New dependencies: `agent-client-protocol` and its schema crate (Apache-2.0), and `futures` with
  its executor. No tokio, no rmcp, no clap in the new crate.
- basemind now carries two protocol adapters that track upstream CLIs with no stability promise.
  They will break when Claude Code or Codex change their wire format. The recorded-transcript tests
  show what the adapters expect, not what the CLIs currently send.
- Verified on 2026-11-16 against Claude Code 2.1.325, Codex 0.169.0 and OpenCode 2.3.0, in a
  throwaway repository through the crate's opt-in `live_probe` example and raw captures; the
  crate README lists what each showed, and the redacted frames are test fixtures. A Claude Code
  launch on the test machine, which has user plugins and claude.ai connectors, passed the init
  check. Not run live: Cursor, Gemini CLI, Grok and Hermes (their flags and mode names are from
  documentation and monocode), OpenCode's plan mode beyond `session/set_mode` being accepted, the
  legacy `applyPatchApproval` (its `fileChanges` comes from Codex's generated schema), and Claude
  Code's managed settings and user `CLAUDE.md`. A Claude Code version whose init frame lacks the
  mode or the servers still fails every launch.
- **User-level configuration still applies** to every harness but Claude Code: the child inherits
  `HOME` and `XDG_*`, so `~/.codex/config.toml` (and `~/.codex/.env`, which Codex 0.169 reads),
  OpenCode's global configuration (whose MCP servers and plugins started although `session/new`
  named none), `~/.gemini/settings.json`,
  `~/.cursor/cli-config.json` and Hermes's settings can approve tools, add MCP servers or run
  hooks behind the policy; Codex 0.169 started the user's own MCP servers and plugins next to the
  injected one. OpenCode's inline `permission: ask` overrides its config files' permissions, but
  its user MCP servers and plugins still load. A launcher-owned config home per harness would
  close this; it is not done because none of those homes was verified to keep sign-in working.
  Claude Code may still read managed (policy) settings and `~/.claude.json`.
- **Default modes that do not ask.** ACP agents other than OpenCode run in modes nobody here
  verified, under an explicit opt-in. Codex `ReadOnly` uses approval policy `never` with a
  read-only sandbox: nothing is asked, whatever the sandbox blocks fails, and an MCP tool call
  fails rather than runs (Codex 0.169). Claude Code's default mode ran `Read`, a read-only `Bash`
  command and `ToolSearch` unasked inside the working directory; `ls /usr` asked (2.1.326).
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
  read its launcher's, and post as it over comms. The supervisor takes neither the id, nor a
  principal, nor a token on its word: depth, fan-out, ownership and audit rest on its own
  records and on the peer process the kernel reports. What remains, all of it same-user:
  - any process of the user outside the supervisor's tree is, to the supervisor, whatever
    principal it names, and can be minted the operator by an engine that accepts it; the
    operator channel is the only fence;
  - a launched agent's process that double-forks away from the harness (reparented to init, its
    ancestry gone) and was never recorded as a detached process looks like such a process,
    unless the daemon is a child subreaper (Linux only; macOS has no equivalent);
  - an ancestry walk ends at a process of another user (macOS does not describe root's
    processes), so a chain through a setuid program such as `sudo` is cut there;
  - a reported peer pid names the connecting process only while it lives: one reused before
    `Peer::of_socket` reads its start time would be taken for it;
  - the token is readable by any process of the user (`/proc/<pid>/environ` on Linux); it is
    honoured only from its own session's processes;
  - a session id or pid reused within a session's 2 s reap window (Linux does not reuse a pid
    still in use as a session or group id; macOS was not checked) could be taken for the
    session's.

  Containing the user's other processes needs a separate user or sandbox.
- **Project files race the check.** The project-file check is a snapshot at plan time: a file
  written between the plan and the harness reading its configuration, or during the session, is
  only seen by the next launch, which it then refuses. This repository has a `.codex/`, so Codex
  refuses to launch in it.
- **Per-session memory.** Bounded, but not small: two queued frames and the one being read (each
  up to 16 MiB and 256 Ki JSON values, a few tens of MiB once parsed), 16 MiB of queued events,
  16 pending requests of up to 1 MiB, 16 Codex file changes of up to 1 MiB, 64 tracked tool calls
  of up to about 192 KiB and 8 MiB of text de-duplication: on the order of 200 MiB in the worst
  case. The policy's `max_concurrent` caps the sessions of one supervisor (at most 32), and the
  engine runs one supervisor per daemon.
- **Host preconditions.** The launcher must not ignore SIGCHLD or set `SA_NOCLDWAIT` (a launch is
  refused then), and must not reap children it did not start (`waitpid(-1)` elsewhere in the
  process): a leader reaped behind the session's back is never signalled again, and its group is
  left alone. SIGPIPE must be ignored, as Rust binaries do by default, or a write to an exited
  harness kills the launcher.
- Launching is Unix-only: the crate refuses to start a harness on Windows until its process tree
  is contained by a Job Object, which is not implemented. A Windows plan still refuses a batch file (`claude.cmd`, run
  through `cmd.exe`, which looks for `node` in the working directory first) and sets
  `NoDefaultCurrentDirectoryInExePath=1`, so a bare name the harness runs (`git`, `rg`) is not
  taken from the agent's working directory; neither was run on Windows.
- A descendant that leaves the process group (a helper that daemonizes) survives a `Session`. The
  session still ends on time and stops reading its pipes; containing such a process needs a cgroup
  or a job object. `opencode acp` 2.3.0 does this by design: it runs its server (`opencode serve
  --stdio`), the MCP servers and plugins in a process group of their own, which exit once the
  server's input closes, not when the session's group is killed: in a live probe they outlived a
  kill of the leader's group by about 1.7 s with its input closed, and by about 3 s (once
  more than 3 s, gone within 13 s) with it open. The keeper does not reach them. The
  `Supervisor` does, for a flavor with `Caveat::DetachedServer`, when it saw them: a process that
  left before the first 200 ms listing, or started and left between two, is missed, and so is
  anything another flavor's harness detaches. A group's members are checked right before it is
  signalled, but a group id freed and reused in that instant would be signalled (a single process
  is signalled through a pidfd on Linux, which closes that gap). A launcher that dies outright
  leaves them to exit on their own once their input closes.
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

## Open questions, with recommendations

The owner decides; each recommendation is what the crate now implements or what the engine
should do next.

1. **Who may launch?** Recommended and implemented: only principals the configuration names,
   with launching off by default. A launched agent may launch only while `max_depth` allows,
   identified by its peer process (its session) and its launch token; the recommended default is
   `max_depth = 1`, which refuses every launch by a launched agent.
2. **Is the launched agent a sub-identity or a principal?** Recommended and implemented: a
   principal of its own, `launched-<flavor>-<id>-<hex>`, with its launcher recorded in the
   supervisor and the audit log, never taken from what the agent reports. The grammar's `.`
   sub-identity stays for `as` within one session: a sub-identity of the launcher would let the
   launched agent read and post as its launcher's principal (ADR-0021 rules 5 and 6).
3. **Should a human approve each launch?** Recommended, not implemented: yes for a flavor whose
   policy names a bypass mode or whose config carries an `UnverifiedModeOptIn`, and for a
   flavor with `Caveat::UserMcpServers` until a launcher-owned config home closes it; no for
   Claude Code and Codex in supervised mode within the policy's limits. The engine can hold such
   a launch as a pending comms request and call `Supervisor::launch` on the operator's answer;
   the crate needs no change for it.
4. **Budget.** Implemented: wall time, turns, depth, fan-out, live sessions and a lifetime
   launch count. Recommended: add a token or cost budget only once the adapters report usage in
   one shape; the three harnesses report it differently and the events drop it today.
5. **Pre-approving basemind's own tools.** Recommended: no pre-approval of domain tools by name;
   the injected server runs a restricted serve profile with the comms verb tools and
   `agent_launch` / `agent_stop` only, and the approval policy decides on those.
6. **Windows.** Recommended: keep `agent_launch` refused on Windows until a Job Object contains
   the tree and has a test. The supervisor's detached-group tracking is Unix-only too.
7. **Where does the audit log live?** Recommended: `<data dir>/agents/launch-audit.jsonl`, one
   per machine beside the daemon's state, rotated by the operator; the engine opens it with
   `FileAuditSink::open` at daemon start and refuses to enable launching when it cannot.
