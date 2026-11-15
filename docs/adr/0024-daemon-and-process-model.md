---
status: accepted
date: 2026-11-15
amends: ["0013"]
---

# ADR-0024: Daemon and process model — rendezvous scoped by protocol generation

- **Deciders:** basemind maintainers
- **Related:** amends ADR-0013 ("`ensure_daemon` replaces an older daemon on the version check");
  ADR-0016, ADR-0021, ADR-0023, ADR-0025, ADR-0026

## Context

basemind runs one background daemon per operating-system user. It holds the Fjall databases, which
admit one process per directory, and brokers agent messages. Its clients are the `basemind serve`
stdio relays that MCP hosts start, CLI commands and plugin hooks. They find it through one fixed
rendezvous in the comms directory (`src/comms/singleton.rs`, `src/daemon_lock.rs`): a Unix socket
`comms.sock` (on Windows a named pipe named from a hash of the comms directory path), an exclusive
file lock `daemon.lock` with a `daemon.pid` beside it, and the agent message store.

Several basemind versions are normally installed on one machine at once: the plugin launcher
(`scripts/mcp-launch.sh`) downloads the binary matching the plugin manifest's version into a
per-version cache directory, and the same user may also have a Homebrew, npm or pip install.

Two mechanisms on `main` make versions destroy each other:

- **The client replaces a foreign daemon.** In `ensure_daemon_with_spawn_policy`
  (`src/comms/singleton.rs`, the `compatible` test) a daemon is reused only when its protocol
  version equals the client's and its release is not older. Otherwise the client sends `Stop`,
  waits three seconds, then signals the daemon's pid with SIGTERM and, after five seconds, SIGKILL.
  A daemon with a **newer** protocol also fails the test, so an old binary kills a new daemon, and
  with two protocol versions in use each side's next session start kills the other's. The pid
  comes from the status reply sent before the wait; `force_terminate` tests only that it is live.
- **The launcher reaps other versions.** `reap_other_versions` (`scripts/mcp-launch.sh`, lines
  78-101, called at lines 184, 344 and 413) sends TERM and, within two seconds, KILL to every
  process whose executable lies under the launcher's cache but not under the version it is about
  to start, daemons included, matched by path with no lock check. `prune_stale_versions` (line 60)
  then deletes every other version's directory. Unix only.

Index ownership on `main`: a CLI `scan` opens the workspace store itself under the workspace writer
lock (`open_store_for_write` in `src/main.rs`) and stops with a notice when another process holds
it; `rescan` forwards to a daemon when one answers (`daemon_is_up` in `src/cli/rescan.rs`, which
never starts one) and scans locally otherwise. ADR-0016 already lets `basemind scan` write when no
daemon runs.

## Decision

1. **The rendezvous is scoped by protocol generation.** The protocol generation is its own number
   compiled into the binary (the rendezvous generation constant in `src/comms/singleton.rs`),
   separate from `PROTO_VER`. It changes exactly when builds must no longer share a daemon. The
   socket, the daemon lock and pid, and the agent store live in `<comms dir>/g<N>/`, also when
   `BASEMIND_COMMS_DIR` overrides the comms directory. The Windows pipe name is derived from that
   directory's path and so includes the generation. Binaries of different generations never connect
   to, probe or stop each other, and each generation in use runs its own daemon. Releases up to
   0.33 use the comms directory itself; no later binary touches that rendezvous.
2. **Within a generation a client never kills a newer daemon.** A daemon of the same or a newer
   release is used as it is. A `PROTO_VER` mismatch still fails at `Hello`. A client may ask an
   older daemon of its generation to stop so that the newest binary takes over.
3. **A refused stop is honoured.** A daemon refuses `Stop` while anything keeps it from its idle
   exit: another connected client, work of its own in flight, or an HTTP request in flight (the
   conditions of `is_idle_for` in `src/comms/daemon.rs`; on `main` `Stop` is never refused). The
   client then uses that daemon if their protocols match and otherwise reports that an older
   daemon is busy. It does not escalate past a refusal.
4. **Signals are for an unresponsive daemon only.** A client signals a process only when the
   endpoint does not answer at all, and only the process that the generation's lock identifies as
   its holder, checked immediately before the signal.
5. **One writer per workspace index at a time,** enforced by the workspace lock. A running daemon
   is that writer, and CLI commands forward their writes to it. With no daemon running, a CLI
   `scan` writes directly, as ADR-0016 allows. A CLI command does not start a daemon just to scan.
6. **The launcher never signals another version's processes.** It prunes only version directories
   that no process is running from. Removing `reap_other_versions` is part of the coexistence work.
7. **Protocol generations are retired by disuse.** A daemon exits when idle. No daemon or client
   touches another generation's rendezvous while its lock is held. `basemind agent cleanup`
   removes a generation directory whose lock is free and whose last use is older than 30 days. For
   the pre-generation rendezvous, which is the comms directory itself, it removes the socket, lock,
   pid and `store.fjall` under the same test, not the generation directories beside them.
8. **Amendment of ADR-0013.** Its "`ensure_daemon` replaces an older daemon on the version check"
   holds only inside a protocol generation and only as rules 2 to 4 allow.

An implementation is in progress on the branch `fix/daemon-coexistence`; this record fixes the
rules it is held to.

## Consequences

- **Two daemons while two protocol generations are in use.** Each holds its own sockets, Fjall
  handles, block caches and worker threads; memory and file-descriptor use roughly double.
- **Old generations linger, but not indefinitely.** A daemon exits ten minutes after it last had a
  client connected (`IDLE_REAP_AFTER`), provided no work is in flight. Callers connect per request,
  so a long-lived session on an old plugin version does not hold its daemon: the daemon goes ten
  minutes after the session's last call, and the next call starts it again.
- **The agent store is per protocol generation.** A store is not carried between generations.
  ADR-0021's never-wiped rule holds within a generation. Agents on different generations cannot
  exchange messages.
- **The other stores are not generation-scoped:** memory notes, git history, vectors and the code
  index are keyed by workspace or repository group. When daemons of two generations want the same
  workspace, the first holds its locks and the second reports that the workspace is held by
  another basemind version. It serves no index, memory notes or git history for that workspace
  until the first daemon exits. If the first is the newer, the older then refuses the newer
  derived stores with an error (ADR-0025) rather than serving from blobs.
- **CLI.** A `scan` with no daemon running works as today, so CI and the harden harness index
  without one. With a daemon running, `scan` must forward where today it stops with a notice.
- **The launcher change costs disk and stragglers.** Old version directories stay while anything
  runs from them. A plugin install that has not updated still runs the old launcher and still
  kills other versions' processes; no new binary can prevent that.
- A daemon that refuses to stop and is in fact broken stays until a person stops it;
  `basemind comms stop` remains the manual route.
- A generation bump is a deliberate act with a visible cost, so protocol changes that older
  clients can survive stay within the generation. Commit `f976d669` is the precedent: it retired
  two requests without a bump, because an undecodable request drops one link only. ADR-0021's
  protocol change also stays within the generation, at the price that an older client is refused
  at `Hello` while a newer daemon runs.
- The machine-wide ceiling of eight live comms daemons (`MAX_LIVE_DAEMONS` in `src/daemon_lock.rs`)
  must count the daemons of every generation together.

## Alternatives considered

- **One daemon that negotiates the protocol version with each client** — rejected: the daemon would
  have to carry every older request type and store layout it might be asked for, and the released
  0.33.0 client does not negotiate: it kills. Coexistence has to come from the two not meeting.
- **A socket per release version** — rejected: every patch upgrade would start a second daemon and
  split the message store, although patch releases are protocol-compatible by policy.
- **No daemon; every process opens the stores itself** — rejected: Fjall admits one process per
  directory, so concurrent sessions in one repository would fall back to blob reads, and agent
  messaging needs a process that outlives a single session.
- **Keep replacing the incompatible daemon, but never a newer one** — rejected: it fixes new
  binaries only. A 0.33.0 client still kills whatever daemon answers on the shared socket.
- **The daemon as the only writer, with no direct CLI scan** — rejected: it contradicts ADR-0016,
  and a machine where the daemon cannot start could then never build an index.
- **Keep the launcher reap** — rejected: it kills by path, daemons of other generations included,
  which defeats rule 1.
- **Carry the agent store into a new generation** when the old lock is free — not adopted: one more
  migration path for chat that retention deletes after 30 days anyway.
