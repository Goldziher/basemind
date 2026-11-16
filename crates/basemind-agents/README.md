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
  the grant itself (command, file changes, permissions, tool input) with an `ApprovalKind` and the
  raw wire params; an agent's reason is only appended. A Codex file change carries the item's full
  changes, or is denied unasked when they are unknown or over 1 MiB. An ACP allow takes
  `allow_once` only. At most 16 requests wait for the caller, under ids the crate assigns.
- A permission-bypass mode is never produced unless the config carries a `BypassOptIn` naming that
  harness and its mode.
- An ACP agent in any other mode needs an `UnverifiedModeOptIn` naming the harness and the mode:
  what it does unasked was not verified. OpenCode is never launched supervised, since its default
  edits files and runs commands without asking.
- Of a request, only the model and the effort reach the command line, as the harness's own flags
  after a grammar check; its wall time and turns are clamped to the config's caps, and its brief is
  at most 1 MiB.
- Claude Code is asked for no setting sources, hooks disabled and only the config's MCP server, and
  the session fails unless Claude Code's init frame reports the requested permission mode and
  exactly that server. Codex, Gemini, Cursor and OpenCode are not launched over one of their
  project files in the working directory or above it. Only the MCP tools the config names are
  pre-approved (Claude Code only).
- The harness executable and the MCP server's command resolve to absolute paths outside the root;
  the child's `PATH` keeps only absolute entries.
- The working directory is relative and has to resolve inside the config's root, and is checked
  again in the child. The agent is not sandboxed to the root.
- The child's environment is `ENV_ALLOWLIST`, the config's additions and the agent identity. An
  addition must be a provider credential or one of a few provider settings, and may not redirect
  configuration, code loading, an endpoint or basemind. Credentials in the launcher's environment
  do not reach the child.
- At the wall-time limit, or when the harness starts more turns than allowed, the process group is
  killed; a stop gets the stop grace. The group is gone before `Event::Finished` is reported.
- Frames, the event queue, channels and tables are bounded; an ACP agent that leaves the client's
  lines unread fails the session. Dropping a session waits at most 2 s for its supervisor, and
  kills the group either way.

Limits:

- The config's executable, its MCP server's command and its environment values are trusted; the
  configuration must restrict them.
- The harnesses' user-level configuration (`~/.codex`, `~/.gemini`, `~/.cursor`, OpenCode's and
  Hermes's) still applies, and so do Claude Code's managed settings and `CLAUDE.md` files.
- What an ACP agent's modes ask for, the fields of Claude Code's init frame and the effect of an
  empty `--setting-sources` were not run against a live harness; the checks fail closed.
- `BASEMIND_AGENT_ID` is self-asserted: a launched agent with a shell can post as anyone.
- A descendant that leaves the process group survives the session.
- A project file written during a session is only seen by the next launch.
- The launcher must not ignore SIGCHLD or reap children it did not start, and must ignore SIGPIPE.
- Nothing is launched off Unix.

## Provenance

The Claude Code and Codex adapters, the process-group handling, the event types and parts of the
ACP dialects are adapted from [monocode](https://github.com/hardbeat920/monocode) (MIT). See `ATTRIBUTIONS.md` and
`LICENSE-monocode`. ACP agents are driven through the
[`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) crate (Apache-2.0).

## License

MIT. See `LICENSE`.
