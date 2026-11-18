---
status: accepted
date: 2026-11-18
amends: ["0024", "0030"]
---

# ADR-0045: Daemon stop and idle rules as built after the comms v2 cutover

- **Deciders:** the repository owner
- **Related:** amends ADR-0030 rules 5 and 6 and ADR-0024 rule 7; ADR-0038 (comms v2 cutover),
  ADR-0039 (forwarded CLI calls)

## Context

ADR-0030 rule 5 lists what makes a daemon refuse `Stop`: a relay session or handshake, an inbox
subscription, an HTTP request or work of its own, and it says a per-request link does not count,
so a read forwarded over one does not hold `Stop` off. Rule 6 lets an operator's
`basemind comms stop` reclaim a daemon that answers `store_error`. Since then:

- Comms v2 (ADR-0038) removed inbox subscriptions. A waiting `agent_inbox {wait_secs}` is a read
  the daemon ends when it drains (`Broker::finish_drain` in `src/comms/daemon_lifecycle.rs`).
- A forwarded CLI command (ADR-0039, `src/cli/forward.rs`) is a relay session, not a per-request
  link, so it counts as one while it runs.
- No daemon answers `store_error` any more: the v2 agent store reports a broken store as
  `agent_store` in `Status` and never takes the daemon down. The reclaim exception, the
  `FailingPredecessor` error and the `last-fatal.json` record were removed.
- A daemon serving the HTTP front-end reaped itself after its idle window. No HTTP client can start
  a daemon, and an implicit spawn withholds the HTTP grant, so HTTP went away until an operator
  started a daemon again.

ADR-0038 amends only ADR-0030 rule 10 and ADR-0039 does not mention `Stop`, so no record states
these rules as they are on `main`.

## Decision

1. **ADR-0030 rule 5 becomes:** a daemon refuses `Stop` (`daemon_busy`) while it has a relay
   session or handshake, an HTTP request in flight, or work of its own (blob GC, a git-history
   sync, a forwarded CLI `rescan`) (`Broker::on_stop`). A relay session counts whichever client
   opened it, so a forwarded CLI command holds `Stop` off while it runs. A waiting `agent_inbox`
   does not: the drain that an accepted `Stop` starts ends it. A relay link counts from its accept
   and must send its hello within 30 seconds. A rescan forwarded after an accepted `Stop` gets
   `rescan_draining`.
2. **ADR-0030 rule 6 becomes:** a takeover never escalates past a refusal, and reports the daemon
   as busy or stale (answers `Ping`, not `Stop`). Any refusal, whatever its code, is honoured by
   `basemind comms stop` too; `--all` exits non-zero when any daemon refused or stayed.
3. **ADR-0024 rule 7, "a daemon exits when idle", holds except for a daemon serving HTTP.** Once
   its HTTP front-end listens, neither the idle reaper nor the bootstrap reaper stops it
   (`Broker::serves_http`); it runs until `Stop`, a signal or the loss of its socket.

## Consequences

- An operator stopping the daemon while a CLI command runs gets `daemon_busy` and retries; the
  command is not cut off.
- An HTTP-enabled daemon is a long-lived service. It is started explicitly (`basemind comms start`
  or the systemd unit) and stopped explicitly; it never stops by itself.
- A daemon whose agent store fails stays up and serves code tools. `basemind comms doctor --probe`
  reports it as degraded from its `agent_store` state.

## Alternatives considered

- **Keep ADR-0030 rule 5 and stop counting CLI relays.** A forwarded `rescan` or long read would
  then be cut off by an operator's stop, which is what the refusal exists to prevent.
- **Pass the HTTP grant to implicit spawns.** A daemon serves every session on the machine, so a
  grant inherited from whichever shell started it would apply to all of them; ADR-0030 withholds
  grants from implicit spawns for that reason.
- **Editing ADR-0030.** It is accepted, so it is immutable.
