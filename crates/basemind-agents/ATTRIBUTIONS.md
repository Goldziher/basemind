# Attributions

This file acknowledges the code this crate derives from. It repeats the matching section of the
repository's top-level `ATTRIBUTIONS.md`, so the notice travels with the published crate.

## monocode provider layer (Claude Code and Codex adapters, process groups)

The parts of basemind's agent launch that have no maintained crate: the Claude Code `stream-json`
adapter, the Codex `app-server` adapter, process-group handling, the event, approval and control
types, and the per-agent dialect of the ACP adapter.

- **Source**: <https://github.com/hardbeat920/monocode>
- **Upstream paths**:
  - `crates/monocode-core/src/provider.rs`
  - `crates/monocode-core/src/provider/acp.rs`
  - `crates/monocode-core/src/provider/lifecycle.rs`
  - `crates/monocode-core/src/provider/parked.rs`
  - `crates/monocode-core/src/process.rs`
  - `crates/monocode-core/src/agent.rs`
  - `crates/monocode-core/src/tool.rs`
  - `crates/monocode-core/src/transcript.rs`
  - `crates/monocode-core/src/mono/bridge.rs`
- **Upstream commit**: `09d04436babb39ad72198cdf22f735067027fc04`
- **License**: MIT (Copyright (c) 2026 Nick), copied verbatim to
  `crates/basemind-agents/LICENSE-monocode`
- **Authors**: Nick and the monocode contributors
- **Location**:
  `crates/basemind-agents/src/{claude,codex,line,process,event,session,acp,spec,harness,guard}.rs`,
  and the replayed task-notification frame in `crates/basemind-agents/src/claude_tests.rs`
- **Purpose**: Drive Claude Code and Codex over their stdio protocols, give each ACP agent its
  launch arguments and modes, and end a launched harness together with every process that stayed
  in its process group.

### Modifications

- Each derived file carries a one-line provenance header (source, paths, commit, license).
- monocode's single `Protocol` state machine, which switches on the provider kind, is split into one
  state machine per harness behind a `Protocol` trait (`claude.rs`, `codex.rs`), with the shared
  line loop and the delta/snapshot text de-duplication in `line.rs`.
- Dropped: everything that serves monocode's desktop application: the warm process pool, session
  resume and rewind, context compaction, questions and elicitation forms, background-task tracking,
  usage and rate-limit reporting, attachments, accounts, the Mono and visual bridges, the Pi, Devin
  and OpenCode transports and the hand-written ACP client (replaced by the `agent-client-protocol`
  crate).
- Changed behaviour:
  - an approval request is answered by a policy and denied without one, where monocode auto-approves
    under its full-access mode; it carries the grant, not the agent's reason;
  - an ACP allow selects `allow_once` only, where monocode falls back to `allow_always`;
  - Claude Code gets `--strict-mcp-config`, `--settings` with `disableAllHooks` always, and an
    empty `--setting-sources`, where monocode loads `user,project,local`;
  - model and effort flags are built by the adapter from a request's checked values
    (`guard.rs`), with monocode's flag names;
  - Claude Code's `system/init` frame has to report the requested permission mode and only the
    injected MCP server, and the brief waits for its `initialize` answer to report that mode and
    only built-in commands;
  - Claude Code gets `--disallowedTools` for the tools that start work outside the session;
  - an ACP agent's mode other than an authorized bypass needs an explicit opt-in, and OpenCode
    is not launched in its default mode;
  - a follow-up starts a new turn when the current one ends, where monocode steers the running turn;
  - turns are counted against a limit, and a turn the harness starts unprompted counts too;
  - errors are a typed `LaunchError` instead of `String`.
- `process.rs`: the scheduling hints (`setpriority`, the macOS I/O throttle) and the global registry
  of running children are removed. `retire` is blocking, bounded by a budget, and observes the exit
  with `waitid(WNOWAIT)`, so the process group is always signalled before its leader is reaped and
  the group id cannot have been reused. A launch is refused while SIGCHLD is ignored and off Unix.
  A keeper process, which monocode does not have, kills the group when the launcher dies.
- `agent.rs`: `AccessMode` becomes `PermissionMode` (`spec.rs`); its full-access variant needs an
  explicit opt-in naming the harness and the mode.
- `provider/acp.rs`: Grok's arguments and `GROK_NO_AUTO_UPDATE`, the `yoloMode` `_meta`, and the
  Hermes and OpenCode mode names are kept (`acp.rs`, `harness.rs`); the client itself is not.
- `mono/bridge.rs`: the Codex `-c mcp_servers.<name>.*` overrides are kept, generalized from
  monocode's own MCP server to a caller-described one, without `default_tools_approval_mode`.

### License Compatibility

MIT is permissive and compatible with basemind's MIT license. monocode's `NOTICE` lists third-party
material (Zed's GPUI under Apache-2.0, mermaid-rs-renderer, Hugeicons, CodeMirror, cuelume) that
belongs to its native user interface; none of it is in the files this crate derives from.
