---
status: accepted
date: 2026-11-15
---

# ADR-0024: Daemon and process model — rendezvous scoped by protocol generation

- **Deciders:** basemind maintainers
- **Related:** ADR-0013, ADR-0016, ADR-0021, ADR-0025, ADR-0026

## Context

basemind runs one background daemon per operating-system user. It holds the Fjall databases, which
admit one process per directory, and brokers agent messages. Its clients are the `basemind serve`
stdio relays that MCP hosts start, CLI commands and plugin hooks. They find it through one fixed
rendezvous in the comms directory (`src/comms/singleton.rs`, `src/daemon_lock.rs`):

- a Unix socket `comms.sock`, or on Windows a named pipe whose name is derived from a hash of the
  comms directory path;
- an exclusive file lock `daemon.lock` with a `daemon.pid` beside it;
- the agent message store.

Several basemind versions are normally installed on one machine at once. The plugin launcher
(`scripts/mcp-launch.sh`) downloads the binary matching the plugin manifest's version into a
per-version cache directory, and the same user may also have a Homebrew, npm or pip install. Each
host session starts whichever binary its launcher resolves.

Released 0.33.0 handles a foreign daemon by replacing it. In `ensure_daemon_with_spawn_policy`
(`src/comms/singleton.rs`, the `compatible` test at line 419 of the `v0.33.0` tag) a daemon is
reused only when its protocol version equals the client's and its release is not older. Otherwise
the client sends `Stop`, waits three seconds, then signals the daemon's pid with SIGTERM and, after
a five-second grace, SIGKILL. Two consequences follow from the code:

- A client meeting a daemon with a **newer** protocol also fails the test, so an old binary kills a
  new daemon. With two protocol versions in use, each side's next session start kills the other's
  daemon, and every session attached to the killed daemon loses its in-flight requests.
- The pid that is signalled comes from the daemon's status reply, sent before the wait. Nothing
  re-checks that the pid still belongs to that daemon: `force_terminate` tests only that the pid is
  live.

The code index has a second ownership question. On `main` a CLI `scan` or a `serve` without a daemon
opens a workspace's index itself as the writer, and readers that lose the lock fall back to the
blobs (`IndexRoute` in `src/mcp/index_route.rs`, `src/cli/context.rs`). Which process writes
depends on which started first.

## Decision

1. **The rendezvous is scoped by protocol generation.** A generation is a number compiled into the
   binary. It changes exactly when a daemon and a client from different builds can no longer serve
   each other, that is, when the comms or relay protocol changes incompatibly. The socket or pipe,
   the daemon lock and the comms store directory all live under a location named by the generation
   (constants: see `src/comms/singleton.rs`). Binaries of different generations therefore never
   connect to, probe or stop each other, and each generation in use runs its own daemon.
2. **Within a generation a client never kills a newer daemon.** A daemon of the same or a newer
   release is used as it is. A client may ask an older daemon of its generation to stop so that the
   newest binary takes over.
3. **A refused stop is honoured.** A daemon may answer `Stop` with a refusal, and the client then
   uses that daemon. A client does not escalate past a refusal.
4. **Signals are for an unresponsive daemon only.** A client signals a process only when the
   endpoint does not answer at all, and only the process that the generation's lock identifies as
   its holder, checked immediately before the signal.
5. **One writer per workspace index: the daemon.** The daemon is the only process that opens a
   workspace's code index for writing. A CLI process forwards its work to the daemon when one is
   running, and otherwise reads the content-addressed blobs directly without opening the index.
6. **Generations are retired by disuse.** A daemon exits when it has had no client for its idle
   period (see `src/comms/daemon.rs`). No binary cleans up another generation's rendezvous.

An implementation is in progress on the branch `fix/daemon-coexistence`. This record fixes the
rules; names, paths and timeouts are whatever `src/comms/singleton.rs` defines.

## Consequences

- **Two daemons while two generations are in use.** Each holds its own sockets, Fjall handles,
  block caches and worker threads. Memory and file-descriptor use roughly double for that period.
- **Old generations linger** until the last session started by an old binary ends and that daemon's
  idle period passes. A long-lived session on an old plugin version keeps its daemon alive
  indefinitely.
- **The agent message store is per generation.** Agents whose binaries are of different generations
  cannot exchange messages, and the first daemon of a new generation starts with no threads.
  ADR-0021 decides that the store is never wiped by a version change; it does not decide how a
  store is carried from one generation's directory to the next. That needs a decision before the
  first generation change after ADR-0021 ships.
- **The code index is not per generation.** Workspace caches are keyed by workspace, so two daemons
  of different generations can want the same index directory. Fjall's lock lets one win; the other
  serves that workspace from blobs. Which formats two releases may share is ADR-0025's subject.
- With rule 5, a CLI `scan` needs a running daemon to write an index. On a machine where the daemon
  cannot start, the CLI reads blobs and cannot build or refresh an index.
- A daemon that refuses to stop and is in fact broken stays until a person stops it;
  `basemind comms stop` remains the manual route.
- A generation bump is now a deliberate act with a visible cost, so protocol changes that older
  clients can survive should stay within the generation. Commit `f976d669` is the precedent: it
  retired two requests without a bump, because an undecodable request drops one link only.
- The machine-wide ceiling of eight live comms daemons (`MAX_LIVE_DAEMONS` in `src/daemon_lock.rs`)
  counts the daemons of every generation together.

## Alternatives considered

- **One daemon that negotiates the protocol version with each client** — rejected: the daemon would
  have to carry every older request type and store layout it might be asked for, and the released
  0.33.0 client does not negotiate: it kills. No new daemon can make an already-released client
  behave, so coexistence has to come from the two not meeting.
- **A socket per release version** — rejected: a generation would change with every patch release,
  so every upgrade would start a second daemon and split the message store, although patch releases
  are protocol-compatible by policy.
- **No daemon; every process opens the stores itself** — rejected: Fjall admits one process per
  directory, so concurrent sessions in one repository would fall back to blob reads, and agent
  messaging needs a process that outlives a single session.
- **Keep replacing the incompatible daemon, but never a newer one** — rejected: it fixes new
  binaries only. A 0.33.0 client still kills whatever daemon answers on the shared socket, so the
  socket cannot stay shared with it.
