# Security Policy

## Supported Versions

Security fixes are applied to the latest release from the default branch.

## Reporting a Vulnerability

Do not open a public issue for a security vulnerability. Use GitHub's
[private vulnerability reporting](https://github.com/Goldziher/basemind/security/advisories/new)
to contact the maintainers privately.

Include the affected version, reproduction steps, impact, and any suggested remediation.

## Threat model

basemind runs as the user who starts it, indexes repositories that user points it at, and answers
the agents that user runs. A finding is in scope when one of the parties below gains something the
user did not grant.

### In scope

- **A malicious repository**: its files and its `basemind.toml`. Cloning and indexing a repository
  must not let it read outside its tree, re-include credential files the exclude floor drops
  (`.env`, keys, `*.pem`), follow a symlink out of the workspace, or bind a network listener.
- **Other local users** of the same machine. They must not reach the daemon, its socket or named
  pipe, the comms directory, the HTTP front-end, the index or the memory notes.
- **A prompt-injected agent** calling the MCP tools. A tool argument must not name a path outside
  the workspace, and a tool must not return what the exclude floor drops.

### Out of scope

- **The same user.** Any process running as that user can already read the repository, the data
  home and the token in `http.addr`. The agent-launch feature (`basemind serve --lead`) bounds what
  a model can do through MCP; it is no sandbox against a process of the same user.
- **A compromised host**, including root and the operator's own environment.
- **What an operator grants.** A grant lifts its default by design; see
  [Operator grants](#operator-grants).
- **The root guard.** Refusing a workspace root that is neither a git repository nor holds a
  basemind config prevents an accidental whole-filesystem scan; it is not an authorization
  boundary (`src/config/root_guard.rs`).

## The boundaries

- **Repository config asks, the operator grants.** A repository's `basemind.toml` can ask for a
  capability that reaches outside the repository, but only a `BASEMIND_ALLOW_*` variable in the
  operator's environment grants it (`TrustPolicy`, `src/config/engine.rs`). Without the grant the
  setting is ignored with a warning.
- **A daemon started implicitly drops the grants.** When a session or a CLI call starts the
  daemon, `BASEMIND_ALLOW_ANY_ROOT`, `BASEMIND_ALLOW_EXTRA_ROOTS`, `BASEMIND_ALLOW_DEPENDENCY_ROOTS`,
  `BASEMIND_ALLOW_FOLLOW_SYMLINKS`, `BASEMIND_ALLOW_REPO_CREDENTIALS`, `BASEMIND_ALLOW_HTTP` and
  `BASEMIND_HTTP_ADDR` are removed from its environment; only `basemind comms start` passes them on
  (`src/comms/singleton_spawn.rs`).
- **Tool paths stay in the workspace.** A path from a tool call is refused when it is absolute,
  climbs out with `..`, or resolves through a symlink to a target outside the workspace
  (`contained_file`, `src/path.rs`).
- **The socket and the comms directory are owner-only.** On Unix the comms directory is mode `0700`
  and the socket `0600`, and the daemon rejects a peer whose uid differs from its own
  (`src/comms/singleton.rs`, `src/comms/frontend_uds.rs`). On Windows a client refuses a named
  pipe served by a process of another user (`src/comms/pipe_owner.rs`).
- **HTTP is opt-in and needs a bearer token.** The streamable-HTTP front-end binds only when
  `BASEMIND_ALLOW_HTTP` is set in the daemon's environment, on loopback unless the operator sets
  `BASEMIND_HTTP_ADDR` to another address. Every request presents `Authorization: Bearer <token>`,
  checked before routing. The token is drawn from the OS random source at daemon start and
  published in `<comms_dir>/http.addr`, mode `0600`. On loopback a request whose `Host` is not a
  loopback name is refused, against DNS rebinding. The front-end serves plain HTTP with no TLS: on
  a non-loopback address the bearer token crosses the network in cleartext, and anyone on the path
  can read it and replay it while the daemon runs. Keep the listener on loopback, or put remote
  access behind a channel with authenticated encryption (an SSH tunnel, a VPN, a TLS proxy). Design
  notes: `src/comms/http_auth.rs` and `src/comms/http_frontend.rs`.
- **The HTTP `?agent=` parameter is asserted by the caller.** It sets the memory-owner key of the
  request, and only the bearer token bounds who may set it (`src/comms/http_frontend.rs`). A
  server on that path neither advertises nor serves the agent comms tools (`from_shared`,
  `src/mcp/mod.rs`).
- **Agent launch is off unless the user's own `agents.toml` turns it on.** The file is refused
  unless it is a regular file owned by the user and writable by nobody else; a repository, the
  environment, a flag or a tool argument cannot enable it (`src/lead/policy_file.rs`).

## Operator grants

The grants and what each one lifts are the Grants table of
[`docs/manual/environment.md`](docs/manual/environment.md#grants) (`basemind manual environment`).
A finding that needs one of them set is in scope only when the grant lifts more than that table
says.

## Downloaded grammars and models

basemind downloads tree-sitter grammar libraries, which it loads into its own process, and model
weights on first use. [`ATTRIBUTIONS.md`](ATTRIBUTIONS.md#grammars-and-models-downloaded-at-runtime)
lists what is verified before each is used and where. For grammars:

- A bundle must match the SHA-256 listed in the manifest of the tree-sitter-language-pack GitHub
  release, fetched over HTTPS by default. `TREE_SITTER_LANGUAGE_PACK_MANIFEST_URL` replaces that
  URL, and the replacement may be `http://` or `file://`, so HTTPS protects only the default
  manifest. The manifest is not signed or pinned, and a library is not hashed again when it is
  loaded from the cache.
- The cache is `<data home>/grammars/tree-sitter-language-pack/v<version>/libs` (`BASEMIND_DATA_HOME`),
  or the same suffix under `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR`. On Unix it must be owned by the
  current user or root and not be group- or other-writable, or it is not used.
- `BASEMIND_GRAMMAR_OFFLINE` stops the downloads basemind makes up front (the bootstrap and
  `basemind lang install`); a grammar loaded on demand for another language is still downloaded.
