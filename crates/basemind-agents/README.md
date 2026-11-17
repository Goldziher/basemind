# basemind-agents

Launch a coding agent in another harness: Claude Code, Codex, or any agent that speaks the Agent
Client Protocol (Cursor, Gemini CLI, Grok Build, Hermes Agent, OpenCode). The launcher hands the
agent a brief and an identity and reads a stream of typed events back.

This crate is the provider layer of basemind's cross-harness agent launch. It has no MCP tool, no
CLI and no daemon wiring; those belong to the layers above it.

## Use

A launch has two halves. The `LaunchConfig` is trusted and comes from the operator's
configuration: the harness executable, the root, the MCP server, environment values, opt-ins and the
caps on wall time and turns. The `LaunchRequest` is what a model may choose: the brief, a working
directory inside the root, the model and effort, and limits within the caps.

```rust,no_run
use basemind_agents::{
    AgentIdentity, Event, Harness, HarnessKind, LaunchConfig, LaunchRequest, McpServerSpec, Registry,
};

# fn main() -> Result<(), basemind_agents::LaunchError> {
let mut config = LaunchConfig::new(
    Harness::new(HarnessKind::Codex),
    "/work/repo",
    AgentIdentity::new("codex-reviewer")?,
);
config.mcp_server = Some(McpServerSpec::basemind_serve("basemind"));
let request = LaunchRequest::new("Review the diff on this branch and post your findings to the review thread.");

let session = Registry::with_defaults().launch(&config, &request)?;
for event in session.events() {
    match event {
        Event::Text(text) => print!("{text}"),
        Event::Finished(status) => println!("\n{status:?}"),
        _ => {}
    }
}
# Ok(())
# }
```

## Safety defaults

What the crate enforces against hostile model-chosen fields (the request) and a hostile child. The
config is trusted.

- Every approval request is denied unless the config carries an `ApprovalPolicy`. A request shows
  the grant itself (command, file changes, permissions, tool input, MCP tool and arguments) with an
  `ApprovalKind` and the raw wire params; an agent's reason is only appended. A Codex file change
  carries the item's full changes, or is denied unasked when they are unknown or over 1 MiB. A
  Codex MCP tool call (an `mcpServer/elicitation/request` of kind `mcp_tool_call`) goes to the
  policy like any other request. A Codex command that asks for access beyond its sandbox
  (`additionalPermissions`, `networkApprovalContext`) is an `ApprovalKind::Permissions` request
  whose detail shows that access. An ACP allow takes `allow_once` only. At most 16 requests wait
  for the caller, under ids the crate assigns; two requests sharing a wire id are both denied.
- A permission-bypass mode is never produced unless the config carries a `BypassOptIn` naming that
  harness and its mode.
- An ACP agent in any other mode needs an `UnverifiedModeOptIn` naming the harness and the mode:
  what it does unasked was not verified. OpenCode supervised needs none: every OpenCode launch
  sets `OPENCODE_CONFIG_CONTENT={"permission":"ask"}`. An ACP agent that reports switching itself
  to another session mode, as a mode or as its mode config option (OpenCode's `mode`), fails the
  session. Codex refuses `AcceptEdits`, whose nearest Codex mode
  also runs commands unasked.
- Of a request, only the model and the effort reach the command line, as the harness's own flags
  after a grammar check; its wall time and turns are clamped to the config's caps, and its brief is
  at most 1 MiB and may not start with `/` (a harness runs that as its own slash command). Errors
  about a request's fields do not repeat the root, a path or a whole value, and a working directory
  that does not exist reads the same as one outside the root.
- A failed session reports `Event::Diagnostics`, the full error and the last 8 KiB of the
  harness's stderr, for the launcher's logs only, then `Event::Error`: one line of at most 512
  bytes with control characters replaced and no stderr, which is what a model may be shown.
- Claude Code is launched with `--disallowedTools` for its tools that start or schedule work
  outside the session (`RemoteTrigger`, `CronCreate`, `ScheduleWakeup`, `SendToProject`,
  `PushNotification`, `Workflow`, `EnterWorktree`, ...), and its init frame may offer none of them.
- Claude Code is asked for no setting sources, hooks disabled and only the config's MCP server.
  The brief goes out only once its answer to `initialize` reports the requested permission mode,
  only built-in commands and no agent defined in a `.claude/` of the working directory, its
  ancestors or `HOME`. Its `system/init` frame comes after the brief, so it cannot stop the brief:
  the session is ended, with the harness possibly having started on it, unless that frame
  reports the requested permission mode, exactly that server as loaded from `--mcp-config`
  (`source: "dynamic"`), only built-in plugins, and no plugin (`name:skill`) or `.claude/`-defined
  skill or agent. A project's `.claude/` and `.mcp.json` are not refused: Claude Code 2.1.325 and
  2.1.326 did not load them under these flags, and the checks above catch one that is.
  Codex, Gemini, Cursor and OpenCode are not launched over one of their project files in the
  working directory or above it, and Gemini, Cursor, Grok, Hermes and a generic ACP agent not over
  a `.env*` file there, which could redirect their endpoint or proxy and so their credentials. Only the MCP tools the config names are pre-approved (Claude
  Code only). Claude Code's init frame is checked again every turn. A Codex turn notification
  without the `threadId` its schema requires fails the session, so a sub-agent's turn can neither
  end the session's turn nor escape the turn count.
- The harness executable and the MCP server's command resolve to absolute paths outside the root,
  with no link on the way inside it; the child's `PATH` keeps only absolute entries outside the
  root.
- The working directory is relative and has to resolve inside the config's root, and is checked
  again in the child. The agent is not sandboxed to the root.
- Git run by the harness itself, outside any sandbox it gives the agent, is kept from running
  the repository's programs: every child gets `core.fsmonitor=false` through `GIT_CONFIG_COUNT`,
  and a repository whose own `config` or `config.worktree` sets a filter, a diff driver, an
  external diff or an include is refused (Codex 0.169 ran a clean filter by `git status` when a
  turn started; a Git LFS repository with a local `filter.lfs` is refused too).
- The root may not contain the launcher's `HOME`, `XDG_CONFIG_HOME` or `XDG_DATA_HOME`, and may not
  be, contain or lie inside a harness's directory (`~/.claude`, `~/.codex`, `~/.gemini`, ...) or
  basemind's store and comms directories: a harness lets the agent work in its root.
- The child's environment is `ENV_ALLOWLIST`, the config's additions and the agent identity. An
  addition must be a provider credential or one of a few provider settings, and may not redirect
  configuration, code loading, an endpoint or basemind. Credentials in the launcher's environment
  do not reach the child, and `Debug` output names variables without their values and counts the
  MCP server's arguments without printing them.
- Wall time is counted on a clock that runs while the machine sleeps. At the wall-time limit, or when the harness starts more turns than allowed, the process group is
  killed; a stop, and a session that failed, get the stop grace. The group is gone before
  `Event::Finished` is reported. Requests a stop declines are reported withdrawn, and so is
  every request still pending when the session ends any other way (a failed or exited harness).
- Frames (16 MiB and 256 Ki JSON values each), the event queue, channels and tables are bounded;
  an ACP agent that leaves 16 of the client's lines unread fails the session. Dropping a session
  waits at most 2 s for its supervisor, and kills the group either way.
- A launcher that dies without ending its sessions (SIGKILL, a crash) takes their groups with it:
  each harness group holds a keeper process, forked between fork and exec, that waits on a pipe
  only the launcher holds open and kills its group when that pipe closes. The same mechanism
  serves Linux and macOS; `PR_SET_PDEATHSIG` is not used, as it fires when the spawning thread
  exits and reaches only the leader.

Limits:

- The config's executable, its MCP server's command and its environment values are trusted; the
  configuration must restrict them. The executable check is not atomic with the exec, and outside
  the root is not the same as not writable by the agent.
- The harnesses' user-level configuration (`~/.codex`, `~/.gemini`, `~/.cursor`, OpenCode's and
  Hermes's) still applies: Codex 0.169 started the user's own MCP servers and plugins next to the
  injected one. Claude Code's managed settings and user `CLAUDE.md` were not checked.
- What Cursor's, Gemini's, Grok's, Hermes's and a generic agent's modes ask for, and what
  OpenCode's plan mode blocks, were not run against a live harness.
- `BASEMIND_AGENT_ID` is self-asserted: a launched agent with a shell can post as anyone.
- A descendant that leaves the process group survives the session. `opencode acp` starts its
  server (`opencode serve --stdio`) in a process group of its own, with the MCP servers and plugins
  under it; it exits once its input closes, not when the group is killed (in a live probe about
  1.7 s after the group with its input closed, about 3 s with it open). OpenCode's global
  configuration started its own MCP servers although `session/new` named none.
- A project file written during a session is only seen by the next launch.
- The launcher must not ignore SIGCHLD or reap children it did not start, and must ignore SIGPIPE.
- Nothing is launched off Unix. On Windows a plan already refuses a batch file (an npm `.cmd`
  shim, which runs through `cmd.exe` and finds `node` in the working directory first) and gives
  the child `NoDefaultCurrentDirectoryInExePath=1`; configure the program the shim starts.

## Verified against live harnesses

Run on 2026-11-16 in a throwaway repository with the opt-in example
(`BASEMIND_AGENTS_LIVE=1 cargo run -p basemind-agents --example live_probe`) and with raw
protocol captures. Recorded frames, redacted, are the fixtures in `tests/fixtures/live/`.

- **Claude Code 2.1.325.** It accepts `--setting-sources=` with `--settings
  {"disableAllHooks":true}`, `--strict-mcp-config` and `--mcp-config`. Under them a project
  `CLAUDE.md`, `.claude/settings.json` hooks and permission rules, `.mcp.json`,
  `.claude/agents`, `.claude/commands`, `.claude/skills`, user plugins and claude.ai connectors
  were not loaded; without them all were. `system/init` (sent after the first prompt, and again each turn) carries
  `permissionMode` and `mcp_servers` as `{name, status, source}`; the injected server is `source:
  "dynamic"`. Approvals arrive as `control_request` `can_use_tool` with a fresh UUID per request;
  a deny answers the tool call with an error result and the turn goes on. In the default mode
  `Read`, a read-only `Bash` (`ls`) and `ToolSearch` ran unasked; `Write` and the MCP tool asked.
  A prompt of `/context` ran as a slash command.
- **Claude Code 2.1.326.** `--disallowedTools` with a comma-joined list removed all thirteen listed
  tools from `system/init`'s `tools`. Under the adapter's flags no user or project skill or agent
  was listed, and the `initialize` answer reported `current_permission_mode` and only commands
  marked `builtin`. `ls /usr` through Bash asked (`blocked_path`), unlike `ls` inside the
  working directory. No frame but `system/init` carried `permissionMode`.
- **Codex 0.169.0** (`codex app-server`). `-c key=value` after `app-server` is accepted.
  `item/fileChange/requestApproval` carries `itemId` and no changes; the `fileChange` item
  announced before it carries them. `applyPatchApproval` requires `fileChanges` (generated schema).
  MCP tool calls ask through `mcpServer/elicitation/request`; `accept` with no `persist` ran the
  tool once. The command approval's escalation fields (`additionalPermissions`,
  `networkApprovalContext`, `kind: writeStdin`) are from the schema `codex app-server
  generate-json-schema --experimental` printed; no live request carried them. Under approval policy `never` an MCP call failed instead of running. A project
  `.codex/config.toml` was not loaded in an untrusted project (skills still load); it is in a
  trusted one, so the check stays.
- **`.env` and `bunfig.toml` (2026-11-17).** Claude Code 2.1.326 and OpenCode 2.3.0 are
  Bun-compiled; neither ran a working directory's `bunfig.toml` preload nor read its `.env` or
  `.env.local`, and Codex 0.169 did not read a working directory's `.env` (it reads
  `$CODEX_HOME/.env`, which is user configuration). Gemini CLI's `.env` lookup is from its
  documentation and was not run.
- **OpenCode 2.3.0** (`opencode acp`) speaks ACP on stdio. `session/new` returns no `modes`; its
  mode is the `mode` config option (`build`, `plan`), and `session/set_mode` `plan` is accepted.
  By default it wrote a file unasked; under `OPENCODE_CONFIG_CONTENT={"permission":"ask"}` it asked
  before an edit, a shell command and an MCP call, even with a config file allowing them.

## Provenance

The Claude Code and Codex adapters, the process-group handling, the event types and parts of the
ACP dialects are adapted from [monocode](https://github.com/hardbeat920/monocode) (MIT). See `ATTRIBUTIONS.md` and
`LICENSE-monocode`. ACP agents are driven through the
[`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) crate (Apache-2.0).

## License

MIT. See `LICENSE`.
