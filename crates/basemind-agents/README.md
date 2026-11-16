# basemind-agents

Launch a coding agent in another harness: Claude Code, Codex, or any agent that speaks the Agent
Client Protocol (Cursor, Gemini CLI, Grok Build, Hermes Agent, OpenCode). The launcher hands the
agent a brief and an identity and reads a stream of typed events back.

This crate is the provider layer of basemind's cross-harness agent launch. It has no MCP tool, no
CLI and no daemon wiring; those belong to the layers above it.

## Use

```rust,no_run
use basemind_agents::{AgentIdentity, Event, Harness, HarnessKind, LaunchSpec, McpServerSpec, Registry};

# fn main() -> Result<(), basemind_agents::LaunchError> {
let mut spec = LaunchSpec::new(
    Harness::new(HarnessKind::Codex),
    "/work/repo",
    "Review the diff on this branch and post your findings to the review thread.",
    AgentIdentity::new("codex-reviewer")?,
);
spec.mcp_server = Some(McpServerSpec::basemind_serve("basemind"));

let session = Registry::with_defaults().launch(&spec)?;
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

- Every approval request is denied unless the spec carries an `ApprovalPolicy`.
- A permission-bypass mode is never produced, and a bypass flag among the extra arguments is
  refused, unless the spec carries a `BypassOptIn` naming that harness and its mode.
- The working directory has to resolve inside the spec's root.
- At the wall-time limit, or when the harness starts more turns than allowed, the whole process
  group is killed. It is also gone before `Event::Finished` is reported in every other case.
- The child's environment is `ENV_ALLOWLIST`, the spec's additions and the agent identity.
  Credentials in the launcher's environment do not reach it.

## Provenance

The Claude Code and Codex adapters, the process-group handling and the event types are adapted from
[monocode](https://github.com/hardbeat920/monocode) (MIT). See `ATTRIBUTIONS.md` and
`LICENSE-monocode`. ACP agents are driven through the
[`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) crate (Apache-2.0).

## License

MIT. See `LICENSE`.
