---
status: accepted
date: 2026-11-18
amends: ["0039"]
---

# ADR-0040: Cross-harness agent launch

- **Deciders:** basemind maintainers
- **Related:** amends ADR-0039 (two tools are never forwarded); ADR-0021 (agent comms v2, which
  left two constraints for this ADR), ADR-0018 and ADR-0020 (verb tools), ADR-0023 (crate
  layering), ADR-0024 (daemon and process model)

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
   also in the crate, in its `Supervisor` (decision 6). The third is basemind's wiring, `serve
   --lead` and the operator's policy file (decision 7).

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
     outside the root. A `Supervisor` takes absolute paths only (`RelativeExecutable`): a `PATH`
     entry the user can write (`~/.local/bin`, an npm global bin) is one a launched agent can
     plant a `claude` in for the next launch.
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
     named. Every caller carries a `Peer`, the process the kernel reports for its connection,
     pinned to the process that connected (the pid version in its audit token,
     `LOCAL_PEERTOKEN`, on macOS; a pidfd, `SO_PEERPIDFD`, on Linux 6.5+; only `SO_PEERCRED`'s
     pid on older Linux); a caller cannot be built without one. The supervisor decides by that
     process, not by the claim, and trusts nothing by default.
     Each harness is started with `setsid`, so it leads a POSIX session of its own and nothing it
     starts can join a process group of another session (`setpgid` across sessions is `EPERM`).
     A peer in a live session (by session id, by ancestry read from `/proc` or `proc_pidinfo`,
     or as a detached process recorded for it) is that session whatever principal it claims, at
     the depth the supervisor recorded, and a token it presents has to be that session's own.
     The embedding binds each configured launcher to one process, write-once (`role_bound`)
     until it releases the binding with `unregister_launcher`, also after that process exits:
     the supervisor's own process (`register_launcher_in_process`, admitting that process only,
     not what descends from it), or a child the embedding spawned and still holds
     (`register_launcher_child(&mut Child)`, refused once `try_wait` reports it exited, bound by
     pid and start time). Nothing is ever bound to the peer of a connection: any process of the
     user, an escaped agent's included, can connect to a socket the user can, and an orphan of
     an earlier run is already waiting when the process starts. Neither a process of a session
     nor an ancestor of the supervisor (where a Linux subreaper would adopt a session's orphan)
     can be registered. A peer is a launcher only when it is the registered process or, for a
     child, a descendant of it, read link by link with each parent no younger than its child.
     The operator is bound to the supervisor's own process only (`register_operator_in_process`)
     and made only by `Supervisor::operator()` in that process: no descendant of a registered
     child and no connection is ever the operator. Every other peer is refused: under no
     registered process, or with an ancestry that stops at a process that cannot be read, it is
     `peer_unverified`; below the supervisor in no live session it is `caller_not_admitted`. A
     launched agent's process that escaped its session (orphaned past the session's end,
     double-forked into a session of its own, or started for it by tmux, launchd or ssh)
     descends from init, a subreaper or that spawner, none of which can be registered; it
     descends from a registered process only when a registered child is itself such a spawner,
     which is why a registered child must start nothing on another agent's request. Otherwise
     it is nobody. The operator stops and answers any session and launches none. The env
     identity is self-asserted (ADR-0021 rule 4) and is never consulted.
   - **Identity of a launched agent.** A principal of its own, `launched-<flavor>-<id>-<8 hex>`,
     exported in `BASEMIND_AGENT_ID` with the config's host, its depth in
     `BASEMIND_AGENT_DEPTH` and 256 bits of the operating system's randomness, as hex, in
     `BASEMIND_LAUNCH_TOKEN`. All three pass to the injected MCP server. No config can set a
     `BASEMIND_*` variable, so a config cannot forge them.
   - **Limits.** `max_depth` (at most 8), `max_concurrent` live sessions (at most 32, about 6 GiB
     of worst-case session memory), `max_children` live sessions per launcher, a wall time (at
     most 24 h) and a required budget (`max_launches` and `max_allows` over the supervisor's
     life, `max_turns` per session; no unlimited default). A launch over any of them is refused
     with a typed `SupervisorError` naming the limit (`depth_exceeded`, `concurrency_exceeded`,
     `fan_out_exceeded`, `budget_exhausted`). A session's wall time is the least of the
     request's, the config's and the policy's. A slot is reserved before the harness starts, so
     concurrent launches cannot overshoot; an adapter refusal, a panic in the adapter, a launch
     record that cannot be written and a supervising thread that cannot be started each release
     the slot, and the launch stays spent once its record was written (a request the adapter
     refuses at the plan writes none and spends none). Every allow, the approval policy's or a
     caller's, is spent from the session's allowance (1024) and the supervisor's
     (`max_allows`); with none left the policy's allow becomes a denial before the harness sees
     it and a caller's is refused (`allows_exhausted`). A session that is stopping launches
     nothing.
   - **Audit log.** An `AuditSink` gets one record per registration, launch (written before the
     harness starts), refusal (the policy's or the adapter's), stop, exit and approval decision
     (settled by the policy, by default, withdrawn, or answered by a caller through the
     supervisor, with who answered). A record carries the time, the caller as the supervisor
     verified it (an unverified caller is recorded as nobody, with what it `claimed` and its
     `peer` pid) and its depth, the session, the launched principal, the flavor, the root, the
     depth, the limits, the flavor's caveats and a fixed outcome code; never a brief, an
     approval's command or detail, an environment value, a token or harness text. Records are
     written after the supervisor's lock is released, so a slow disk does not stall the
     sessions. Everything a caller can loop on is coalesced: refusals, a repeated stop, a
     denial or a withdrawal (a caller's or the approval policy's, the latter keyed by session),
     an answer that failed, keyed by the verified caller or, for an unverified one, by
     where its process sits (a session, a registered process, below the supervisor, or
     unverified), never by what it claims and never by outcome, so varying what it gets refused
     buys a caller nothing. 8 a minute per key are written, the rest counted into the next one
     (`suppressed`) or a final record when the supervisor is dropped. A request the adapter
     refuses is checked before a slot is reserved, so it is one refusal and never an accepted
     launch; an approval id the session never issued is refused and not delivered.
     **The reserve.** `FileAuditSink` caps the file (64 MiB by default) and keeps the last
     quarter for the records that must not go unrecorded: launches, exits, the one stop that
     stops each session, registrations and their release, and allows; never a refusal, a denial
     or a repeated stop. `Supervisor::new` claims from the sink (`AuditSink::reserve`), for its
     whole life, the most its budget can write there: three records per launch, two per allow
     and 64 registrations and releases (`registration_limit` past them), each at a bound
     computed from the configs' roots. A sink that cannot set that aside refuses the supervisor
     (`PolicyError::AuditReserve`), so nothing the supervisor is allowed to do can spend the
     reserve, across sessions included. A failed write: a launch whose record cannot be written
     is refused; an allow answered through the supervisor whose record cannot be written is not
     delivered (the request stays pending and is denied when its turn or the session ends); an
     allow the approval policy settled whose record cannot be written stops the session (the
     harness already has it). Any of those marks the log exhausted and the supervisor fails
     closed: every later launch is refused with `audit_exhausted` before anything is tried, no
     allow is given, `Supervisor::audit_exhausted` reports it and the `SessionObserver` hears it
     once, outside the log. Every other record's failure is counted. The engine picks the file,
     one per supervisor: `FileAuditSink` holds it under an exclusive lock, so a second writer
     cannot grow it behind the sink's count. It appends one JSON object per line to a file it
     creates with mode 0600, and refuses a symbolic link, a file with another hard link, a
     non-regular file (opened non-blocking, so a FIFO cannot hang it), another user's file or
     one others can read or write; a symbolic link among the path's directories is followed.
     Past the cap a record is dropped and refused, with an `audit_full` marker and an
     `audit_dropped` count. It is append-only as far as this writer goes, not tamper-resistant:
     any process of the same user can rewrite it.
   - **Sessions.** `status`, `list`, `wait`, `stop` (by the operator, the configured launcher at
     the top of the session's tree, or a session above it, also once the sessions between them
     are reaped, so a grandchild never outlives its parent's entry unstoppable; it reaches every
     session below the one stopped), `answer` for a pending approval (a launcher may
     deny; only the operator allows, unless the flavor's `launcher_may_allow` opt-in says
     otherwise, which `LaunchPolicy::new` refuses with a `max_depth` above 1), `stop_all`,
     `reap`. A thread per session reads its events, hands each to an optional
     `SessionObserver` and audits approvals and the exit; an observer that panics is dropped for
     that session and the session is stopped. Dropping the supervisor stops every session within
     its stop grace, kills what is left and returns.
   - **Per-flavor caveats**, reported on the flavor's policy, on each launch and status, and in
     the launch record. `DetachedServer` (OpenCode): while the session runs the supervisor reads
     the process table every 200 ms and records, by pid and start time, each process that
     descends from the harness (each parent no younger than its child) or sits in a POSIX
     session the harness or a recorded descendant leads while that leader is in the same
     listing; it stops once the harness is gone, and stops the session if it would have to
     record more than 1024 processes or 64 sessions. Once the session ends it waits up to 2 s, then
     kills what is left: a whole group only when every member is the session's and the group is
     neither init's nor the launcher's, else each process alone, each checked again right before
     the signal (through a pidfd on Linux 5.3 and later; before that, and on macOS, a pid reused
     in that instant could be signalled). Nothing is killed without a listing, and the session
     is reported finished only then.
     `UserMcpServers` (OpenCode, Codex): the harness starts the user's own MCP servers and
     plugins from its user-level configuration, OpenCode even with an empty `mcpServers`;
     nothing in the launch switches that off, so an operator allowing the flavor accepts it.

7. **The wiring: only `basemind serve --lead` hosts a supervisor.** Review 9 found that a
   supervisor reached over a socket cannot tell the lead from any other process of the same user:
   the relay hello's principal and client kind are the peer's own claim, so a daemon-hosted
   supervisor would make every same-uid process the lead, including a launched agent's shell and
   the user's own basemind MCP server that Codex and OpenCode start inside a launched session. So:
   - **Topology.** The lead's harness starts `basemind serve --lead`. Its first statement, before
     argument parsing, logging or the runtime, is `basemind_agents::seal_host()`: on Linux the
     process names its thread-group leader `basemind-lead` and makes itself non-dumpable, so no
     process of the user can open its stdin through `/proc` and write the lead's requests into it.
     It then reads the operator's policy, opens the audit log, builds the `Supervisor`, binds it
     with `register_launcher_in_process` and runs the ordinary stdio relay to the daemon (ADR-0024)
     behind a filter. The filter answers `tools/call` of `agent_launch` / `agent_stop` in this
     process and never writes them to the relay; it appends the two tools to the first page of
     `tools/list` and one line to `initialize`'s `instructions`. Every other line passes through
     byte for byte. A line it cannot read as one JSON object (over 1 MiB, not UTF-8, malformed,
     too deeply nested, a JSON-RPC batch) passes through untouched, so a lead tool inside a batch
     reaches the daemon and is answered as unknown.
   - **No other way in.** The daemon never hosts a supervisor and no `BasemindServer` router holds
     the two tools: the daemon's relay sessions (a plain `serve`'s and a CLI's) and its HTTP
     front-end answer them as unknown tools. There is no CLI command for them: a CLI command is a
     forwarded relay session whose launcher identity is the hello's claim, so it would be a
     second way in. They are the one exception to "every MCP tool has a CLI command" (ADR-0018),
     listed as `NOT_FORWARDED` in `tests/cli_parity/capabilities.rs`. This amends ADR-0039: a
     process that does not own the index forwards whole tool calls, except these two, which only
     the lead's own process answers.
   - **No listener.** `max_depth` is 1, so a launched agent never needs to reach a supervisor, and
     nothing listens for one. A launched agent gets `basemind serve` without `--lead` as its MCP
     server (`McpServerSpec::basemind_serve`), which never lists the two tools. If a connection
     path is ever added, it takes the callers' `Peer` from `Peer::of_socket` at accept, refuses
     callers when `!Peer::pins_connections()`, lives in a 0700 directory per `serve` and bounds its
     connections and requests.
   - **Off by default; policy only from the operator's file.** The policy is the `[agents.launch]`
     table of `agents.toml` in basemind's user-level configuration directory
     (`$XDG_CONFIG_HOME/basemind` or `~/.config/basemind` on Linux,
     `~/Library/Application Support/basemind` on macOS). It is opened without following a link
     and refused unless it is a regular file with one link, owned by this user and writable by
     nobody else, in a directory owned by this user or root that nobody else can write, outside
     the lead's workspace. It is never read from the repository's `basemind.toml` (whose `[agents]`
     table is dropped with a warning naming the right file), from the environment, from CLI flags
     or from MCP arguments: `ConfigV1` has no field for it and no override layer can name it.
     Without the file, or with `enabled = false`, the lead is a plain relay: the two tools are not
     listed, a call to either is an unknown tool, and the reason is logged to stderr and appended
     to `instructions`.
   - **What the file can say.** `enabled`, `worktrees_root`, `max_concurrent`, `max_children`,
     `wall_time_secs`, `max_launches`, `max_turns`, `audit_max_bytes`, and per harness
     (`claude`, `codex`, `opencode`) an absolute `executable`, a `permission_mode` of `supervised`,
     `read_only` or `accept_edits`, `pass_env` (credential names whose values are read from the
     lead's environment; a missing one is an error, never a default) and per-session caps. Unknown
     keys are errors. It cannot express a bypass, an `UnverifiedModeOptIn` (so the other ACP
     flavors cannot be configured), an `ApprovalPolicy`, `launcher_may_allow`, `allow_tools`, a
     launcher list or a depth: the lead's comms principal is the only launcher, `max_depth` is 1
     and `max_allows` 0. No operator is registered, since the lead has no interactive elicitation
     channel yet, so every approval a launched agent asks for is denied.
   - **A worktree per launched agent.** `worktrees_root` is the `LaunchConfig::root` of every
     flavor; the builder refuses one that is, contains or lies inside the lead's workspace.
     `agent_launch` requires `working_dir`: an existing directory strictly inside it, plain
     relative components only, not overlapping a directory a live session or a launch in progress
     holds. The crate has no worktree hook, so the lead creates the worktree first (in this
     polyrepo, `mise run worktree:add`).
   - **Audit.** One `FileAuditSink` per lead process, `<data dir>/agents/launch-audit-<pid>-<unix
     seconds>.jsonl`, in a 0700 directory. Launching stays off when it cannot be opened or the
     reserve is refused; `audit_exhausted` is logged on stderr.
   - **Model-facing replies** carry `SupervisorError::code()` (and fixed codes of the wiring:
     `working_dir_invalid`, `working_dir_in_use`, `flavor_not_allowed`), never an error's text or
     `Event::Diagnostics`. `status` and `list` are not exposed.
   - **Unix only.** On Windows `serve --lead` exits with an error; `basemind-agents` is a Unix-only
     dependency enabled by the `comms` feature.

### Engine obligations, as wired

Review 9's conditions, each with where it holds:

1. Topology: only `serve --lead` builds a `Supervisor`, registered in-process before the relay
   reads a request; the daemon, HTTP and CLI answer the tools as unknown
   (`tests/agent_launch_surface.rs`, `tests/http_frontend_smoke.rs`, `src/serve_lead_tests.rs`).
2. `max_depth = 1`, no listener (`lead::policy_file`; a source check in `agent_launch_surface`).
3. Seal first (`seal_if_lead` in `main`; on Linux, `/proc/<pid>` of a lead is root's, its stdin
   cannot be opened and its name is the marker, a plain `serve` being the control).
4. Policy only from the operator file, absolute executables, no forbidden field
   (`src/lead/policy_file_tests.rs`, `tests/config_trust.rs`; `Supervisor::new` also refuses a
   relative executable or MCP command, `RelativeExecutable`).
5. No model allows: no answer tool, no operator registered, `max_allows` 0.
6. `NestedHost` stays on, and the daemon spawn removes `BASEMIND_LAUNCH_TOKEN` and
   `BASEMIND_AGENT_DEPTH`; a lead whose ancestor is named `basemind-lead` is nested even when its
   executable cannot be read, and on Linux an ancestor that cannot be described fails closed.
7. Audit fails closed (`src/lead/host_tests.rs`).
8. Callers: no connection path exists.
9. Unix only.
10. Separate worktree (the builder and `agent_launch` both refuse).
11. Model-facing replies are codes.
12. Budgets are per lead process: see Consequences.

Process hygiene (no `SIGCHLD` disposition change, no `waitpid(-1)`, `SIGPIPE` ignored, the
supervisor dropped on shutdown) holds for `serve`, which installs no signal handler and reaps no
child it did not start. The engine never calls `Registry::launch` directly.

### Tool surface

- `agent_launch { harness, brief, working_dir, model?, effort?, max_turns?, wall_time_s? }` calls
  `Supervisor::launch` for the lead and, on success, opens a comms thread from the lead's principal
  to the launched one (subject `launched <agent>`), returning `session`, `agent`, `depth`,
  `caveats` and `thread` (null with a `notice` when the daemon could not be asked). It takes no
  principal, executable, environment, mode or opt-in. Annotations: not read-only, destructive,
  not idempotent, open-world.
- `agent_stop { session }` calls `Supervisor::stop`; the session's descendants stop with it.

Both are not always-loaded, and they are not verb tools: `tests/fixtures/lead_tools_list.json`
pins their `tools/list` entries under a ceiling of their own, and a plain `serve`'s `tools/list`
is unchanged.

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
  records and on the peer process the kernel reports, and a principal or the operator is
  admitted only from the process the engine registered for it. What remains, all of it
  same-user:
  - a registration is only as trustworthy as the topology above, and a registered child binds
    everything descending from it: a registered spawner admits what it starts, and a
    registered process that executes files a launched agent could write (hooks, build scripts,
    project settings) runs that agent's code as the principal;
  - a process of the user that can trace the registered process (`ptrace`, a debugger), inject
    code into it, or type into it (`tmux send-keys`, `osascript` keystrokes, `TIOCSTI` on Linux
    before 6.2 or with `legacy_tiocsti`) acts as it, where the system allows that;
  - the lead's tool calls are trusted because they arrive on the hosting `serve`'s stdio. On
    Linux any process of the user can open `/proc/<pid>/fd/0` of a dumpable process
    (`PTRACE_MODE_READ`, which Yama does not restrict) and append whole JSON-RPC lines to that
    pipe, so `register_launcher_in_process` and `register_operator_in_process` first set
    `PR_SET_DUMPABLE` to 0 and refuse (`host_dumpable`) if it does not hold: the process's
    `/proc` entries become root's and `ptrace` needs `CAP_SYS_PTRACE`. The flag is reset on
    `exec`, so the harnesses it starts are unaffected. macOS has no `/proc`; attaching to the
    process (`task_for_pid`) is restricted by SIP and the hardened runtime, and an embedding can
    add `PT_DENY_ATTACH`. Launching is refused on Windows;
  - an ancestry walk that stops at a process of another user (macOS does not describe root's
    processes) admits nothing past it, so a registered process has to be reachable without one;
  - on Linux before 6.5 a reported peer pid names the connecting process only while it lives:
    one reused before `Peer::of_socket` reads its start time would be taken for it
    (`Peer::pins_connections` reports whether the host pins, so the engine can refuse callers);
  - the token is readable by any process of the user (`/proc/<pid>/environ` on Linux); it is
    honoured only from its own session's processes;
  - the reaper counts a session id only while its leader is listed, but a pid recorded for the
    session and reused between its last check and the signal could be signalled on macOS and
    on Linux before 5.3 (later Linux signals a single process through a pidfd).

  Containing the user's other processes needs a separate user or sandbox. Put plainly: an agent
  that already runs code as the same user does not need the supervisor. It can run any harness
  itself with its bypass flag, edit basemind's configuration, or stop and replace the daemon
  (ADR-0024 lets any client). The supervisor bounds two things: the MCP path, for agents whose
  harness gates code execution, and the audit's attribution. It does not contain an escapee.
- **What `serve --lead` does and does not bound, same user.** The lead's tool calls are trusted
  because they arrive on its stdio, and `max_depth = 1` holds because no socket reaches the
  supervisor and its stdin cannot be reopened through `/proc` once sealed. Left open:
  - the window from `exec` to `seal_host`, before which a process of the user that was already
    waiting (an orphan of an earlier session) can open `/proc/<pid>/fd/0` for writing; the
    descriptor stays valid after the seal;
  - a process of the user that traces (with `CAP_SYS_PTRACE`), injects into, signals or types
    into the lead's process; on macOS the seal is a no-op and `task_for_pid` is what protects it;
  - the restricted injected `serve` only shapes what a launched harness lists to its model. It is
    no boundary against the agent's own shell: with the daemon up, any process of the user runs
    `basemind <command>` as a CLI relay session whose principal is its own claim, reaching every
    verb tool and `memory_delete` (this predates agent launch). Codex and OpenCode also start the
    user's own basemind MCP server inside a launched session, whose principal falls back to the
    root-derived id, which in a shared root is the lead's;
  - `max_depth` does not stop influence: a launched agent can ask the lead model, over comms or
    in its output, to launch for it. That is bounded only by the lead's `max_children`,
    `max_concurrent` and budget.

  Containing an escaped agent needs a kernel boundary (a separate uid, or a sandbox that denies
  `connect` to the comms socket and access to `/proc/<lead>`), which this ADR does not add.
- **Budgets are per lead process.** `max_launches` (and the unused `max_allows`) live in the
  supervisor, so a lead `serve` that is killed and restarted starts with a fresh budget. Persisting
  them would bound nothing a same-user process cannot already undo: one that can kill the lead
  can edit the operator's file or run a harness itself. A restart is visible as a new audit file,
  which names the process. `max_concurrent` still holds, since the sessions of a killed lead die
  with it (the keeper process in each harness group).
- **Advisory, not enforced:** `BASEMIND_AGENT_ID` and the comms identity of a launched agent;
  that a launched agent stays in its worktree (it is not sandboxed to it); that the lead creates a
  real git worktree for `working_dir` rather than any directory; `UserMcpServers`.
- **Project files race the check.** The project-file check is a snapshot at plan time: a file
  written between the plan and the harness reading its configuration, or during the session, is
  only seen by the next launch, which it then refuses. This repository has a `.codex/`, so Codex
  refuses to launch in it.
- **Per-session memory.** Bounded, but not small: two queued frames and the one being read (each
  up to 16 MiB and 256 Ki JSON values, a few tens of MiB once parsed), 16 MiB of queued events,
  16 pending requests of up to 1 MiB, 16 Codex file changes of up to 1 MiB, 64 tracked tool calls
  of up to about 192 KiB and 8 MiB of text de-duplication: on the order of 200 MiB in the worst
  case. The policy's `max_concurrent` caps the sessions of one supervisor (at most 32), and
  each lead's `serve` hosts one supervisor.
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
  is signalled through a pidfd on Linux 5.3 and later, which closes that gap). A launcher that dies
  outright leaves them to exit on their own once their input closes.
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
   with launching off by default, each bound to the lead's own process, which hosts the
   supervisor (`register_launcher_in_process`). A launched agent may launch only while
   `max_depth` allows, identified by its peer process (its session) and its launch token; the
   recommended default, and the only depth the wiring allows, is `max_depth = 1`. No socket
   reaches the supervisor, so a launched agent's processes, those that escaped its session
   included, can reach it only through the lead's stdin, which on Linux is sealed before the
   first read. It does not stop a process that was already waiting in the window between the
   lead's `exec` and its seal, nor one of the same user that traces (with `CAP_SYS_PTRACE`),
   injects into or types into the lead's process.
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
5. **Pre-approving basemind's own tools.** Implemented: none. The injected server is a plain
   `serve`, which lists neither lead tool; a restricted profile was not added, because it would
   only shape what the harness lists and the agent's shell reaches the full surface anyway.
6. **Windows.** Recommended: keep `agent_launch` refused on Windows until a Job Object contains
   the tree and has a test. The supervisor's detached-group tracking is Unix-only too.
7. **Where does the audit log live?** Implemented: one file per supervisor, so per lead `serve`,
   under `<data dir>/agents/` (`launch-audit-<pid>-<unix seconds>.jsonl`), rotated by the
   operator; the `serve` opens it with `FileAuditSink::open` (which holds it exclusively) before
   it registers, and refuses to enable launching when it cannot open it, when
   `Supervisor::new` refuses the budget for its reserve, or once `audit_exhausted` is set.
