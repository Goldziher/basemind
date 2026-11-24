---
status: accepted
date: 2026-11-18
amends: ["0040"]
---

# ADR-0044: Launched agents use their comms thread, and start none of the user's MCP servers

- **Deciders:** the repository owner
- **Related:** amends ADR-0040 (decision 4 "No tool of it is pre-approved", decision 6
  `UserMcpServers`, the policy file's location under "Off by default; policy only from the
  operator's file", open question 5, the Consequences on user-level configuration and on Codex
  `ReadOnly`); ADR-0021 (agent comms v2)

## Context

ADR-0040 injects `basemind serve` into every launched agent and opens a comms thread from the lead
to it, but pre-approves none of the server's tools. Every approval a launched agent asks for is
denied (no operator is registered and `max_allows` is 0), so the live probes of 2026-11-18
(`reports/lead-live-probes/`) showed the thread is one-way: Claude Code 2.1.325 was denied
`agent_inbox` ("Denied by the launcher's approval policy"), Codex 0.169 answered `user rejected MCP
tool call`, and OpenCode 2.3.0 raised `Permission.DeclinedError`. The agent could not read its
brief's thread nor reply.

The same probes showed Codex 0.169 and OpenCode 2.3.0 starting the user's own MCP servers and
plugins beside the injected one (ADR-0040's `UserMcpServers` caveat). Neither harness lets a launch
replace the user's server table:

- Codex merges `-c mcp_servers=...` into `~/.codex/config.toml` key by key (checked with
  `codex mcp list --json`: a replacing inline table left the user's server listed). `codex exec`
  has `--ignore-user-config`; `codex app-server` has no such flag and its `thread/start` has no
  such field. A separate `CODEX_HOME` keeps sign-in only by moving `auth.json`, whose ChatGPT
  refresh token rotates, so a copy or a link replaced on refresh would sign the user out.
- OpenCode merges `OPENCODE_CONFIG_CONTENT` into its user configuration
  (`$XDG_CONFIG_HOME/opencode/opencode.json{,c}`). Its CLI listings (`opencode mcp list`,
  `opencode debug config`) go through a background service, which they start.

Codex's read-only mode failed in the probes too: the agent's shell could not reach the comms socket
("Operation not permitted"), and its MCP calls failed under approval policy `never`.

## Decision

1. **The operator policy pre-approves the injected server's agent comms tools and nothing else.**
   `serve --lead` sets `allow_tools = ["agent_post", "agent_inbox", "agent_threads"]` (the
   `AGENT_TOOLS` constant) on every flavor's rule and on its injected server; the operator file
   cannot name tools. Each harness's own mechanism:
   - Claude Code: `--allowedTools mcp__basemind__agent_post,mcp__basemind__agent_inbox,mcp__basemind__agent_threads`.
   - Codex: `-c mcp_servers.basemind.tools.<tool>.approval_mode="approve"` per tool, beside the
     server's `default_tools_approval_mode="prompt"` (Codex 0.169 accepts `auto`, `prompt`,
     `writes`, `approve`).
   - OpenCode: `OPENCODE_CONFIG_CONTENT` carries
     `{"permission":{"*":"ask","basemind_agent_post":"allow",...}}`. OpenCode 2.3.2 names an MCP
     tool's permission `<server>_<tool>` and lets the last matching rule win, so `*` comes first.
     It appends every configuration's top-level rules in merge order, the inline one after the
     user's and the project's, so the inline `*` overrides theirs; but it appends an agent's own
     rules (`agent.<name>.permission`, an agent Markdown file's `permission`) after every
     top-level rule, so under a user's `agent.build.permission.bash = "allow"` the last rule
     matching a shell command was that `allow` (`opencode debug agents` lists each agent's rules). The inline configuration therefore gives
     each of the user's agents the same rules, which OpenCode appends after the agent's own.
   - Other ACP agents still refuse a non-empty `allow_tools` (`Unsupported`).

   Every other tool, of the injected server or of the harness, still asks and is denied.

2. **A launched harness starts none of the user's MCP servers.** Each is switched off by name,
   per launch, without moving credentials:
   - Codex: the adapter runs `<codex> -c features.plugins=false -c features.apps=false mcp list
     --json` in the child's environment and working directory before the launch, and starts
     `app-server` with the same two feature switches and `-c mcp_servers.<name>.enabled=false` for
     every listed server. Plugins and ChatGPT apps bring servers of their own; with both off,
     `mcp list` reports the merged table Codex would start (user and managed configuration). A
     listing that fails, times out (15 s), is not a JSON list, or names a server outside
     `[A-Za-z0-9_-]{1,64}` refuses the launch (`LaunchError::UserMcpServers`), as does output a
     process of the listing keeps open past its group's end.
   - OpenCode: the adapter reads every configuration OpenCode 2.3.2 merges beneath the inline
     one (its bundle's `ConfigDiscovery.discover` and `Config.load`; it reads no `config.json`):
     `opencode.json` and `opencode.jsonc` (JSONC comments and trailing commas allowed) in the
     child's `$XDG_CONFIG_HOME/opencode`, else `$HOME/.config/opencode` (an empty variable is
     unset, a relative one is taken against the working directory, and with neither the launch
     is refused), and in every ancestor of the working directory and its `.opencode` directory.
     The project-file check refuses those in every ancestor but the home directory, whose
     `opencode.json` and `.opencode/` OpenCode loads for a working directory under it. Servers are
     the entries of `mcp` (an entry whose `type` is `local` or `remote` whatever its key, else the
     names under `mcp.servers`, OpenCode's canonical form, with `mcp.timeout` a setting and an
     entry naming only `enabled` none); agents are the keys of `agent`, `mode` and `agents` and
     the Markdown files under the configuration directories' `agent/`, `agents/`, `mode/` and
     `modes/`. The adapter adds to `OPENCODE_CONFIG_CONTENT` a whole entry
     `{"type":"local","command":["/usr/bin/true"],"enabled":false}` for each server, which
     replaces the user's entry whole (a remote one was not contacted). OpenCode 2.3.2 kept a
     server whose override named `enabled` alone, and dropped one given the whole entry. A file
     that cannot be read or parsed, or a server or agent name holding `{env:` or `{file:` (which
     OpenCode substitutes in the text before parsing it), refuses the launch.
   - Claude Code: unchanged; `--strict-mcp-config` already loads only the injected server, and the
     `system/init` check fails a session that reports any other.
   - A user server named like the injected one (`basemind`) refuses the launch for Codex and
     OpenCode: its settings would merge into the injected server's.

   Codex loses the `UserMcpServers` caveat. OpenCode keeps it, narrowed to what the launch cannot
   switch off: the user's plugins (OpenCode concatenates `plugin` lists), and the MCP servers and
   permission rules of a managed (system, MDM preferences) configuration, which OpenCode merges
   after the inline one, or of an organisation (`.well-known`) one.

3. **Codex's sandbox does not cover its MCP servers.** `codex app-server` starts them as its own
   children outside the sandbox it applies to the agent's commands, so the injected server reaches
   the comms socket in every mode, and a pre-approved tool runs under approval policy `never`
   (read-only) as under `untrusted` (supervised). The read-only sandbox still blocks the agent's
   shell from the socket (`basemind agent inbox` failed with `Operation not permitted`), which is
   the intended boundary. Nothing is changed for it.

4. **The operator policy's path comes from the user database, not the environment.** ADR-0040
   names `$XDG_CONFIG_HOME/basemind` or `~/.config/basemind` on Linux, which `directories`
   resolves from `HOME` and `XDG_CONFIG_HOME`; an MCP host that sets its servers' environment
   from a project's settings would then choose the policy file, against ADR-0040's "never read
   ... from the environment". `serve --lead` takes the home directory from the user database
   entry of its effective user (`getpwuid_r`) and reads `<home>/.config/basemind/agents.toml`
   (`<home>/Library/Application Support/basemind/agents.toml` on macOS); neither variable moves
   it. Tests that run the binary as a lead name a stand-in home through
   `BASEMIND_TEST_ACCOUNT_HOME`, which only a build with the `test-support` feature reads.

## Consequences

- The thread `agent_launch` returns is two-way: a launched agent reads it and replies through MCP
  without an approval (probes in `reports/lead-live-probes-2/`).
- Through `agent_post` a launched agent can open threads to and message any agent the comms layer
  admits, as its own principal. It could already do so through its shell (ADR-0040: the CLI relay
  takes its principal from the claim); the pre-approval adds no reach that the shell lacks.
- Codex launches run one more short `codex` process before the session, in the same environment
  (the launch token included) and outside the session's process group and keeper: it is the
  executable the session runs, so it is trusted as the harness is. It runs in a session and
  process group of its own, killed whole when it exits or times out, as an npm-installed `codex`
  is a Node shim whose native child holds the output pipe;
  `Registry::plan` for Codex and OpenCode shows the command without the user's servers, which only
  a launch reads.
- The user-server lists are snapshots at launch: a server added to the user configuration between
  the listing and the harness reading it starts. That is the user's own file.
- The user's other Codex and OpenCode configuration (hooks, profiles, providers, approvals rules)
  still applies, as ADR-0040 records.

## Alternatives considered

- **A launcher-owned `CODEX_HOME`** with the user's `auth.json` copied or linked — rejected: it
  moves a credential whose refresh token rotates; a copy diverges, and a link replaced on refresh
  signs the user out.
- **A launcher-owned `XDG_CONFIG_HOME` for OpenCode** — rejected: it drops the user's provider and
  model configuration with the servers, and the agent's shell inherits it (`gh`, git's XDG config).
- **Pre-approving every tool of the injected server** — rejected: only the comms tools are needed
  to use the thread; the code-map tools stay behind the approval like the harness's own tools.
- **Denying the other servers' tools instead of not starting them** — rejected: a server that
  starts runs its own code whether or not a tool is called.
