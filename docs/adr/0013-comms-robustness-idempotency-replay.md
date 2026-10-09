# ADR-0013: Comms robustness: correlated requests, bounded waits, idempotent posts, relay replay

- **Status:** Accepted
- **Date:** 2026-10-09
- **Deciders:** basemind maintainers
- **Related:** ADR-0011

## Context

The comms broker is a singleton daemon shared by every session on the machine. Three failure modes
showed up in practice:

- A client that gave up on a request (a timeout, a cancelled tool call) read the daemon's late reply
  as the answer to its next request.
- Nothing waited with a bound. A wedged or still-booting daemon hung the calling tool, and a slow
  disk stalled `Ping` for every other link because Fjall calls ran on the async workers.
- A daemon restart (upgrade takeover, crash, idle reap) failed whatever the stdio relay had in
  flight, and retrying a `post` blindly stored a second copy.

## Decision

- **Protocol v4.** After `Hello`, every request travels as `Call { id, request }` and is answered by
  `Reply { id, response }`; stale ids are discarded. `Hello` stays bare so a skewed peer still
  decodes it and fails with `proto_skew`; `ensure_daemon` replaces an older daemon on the version
  check.
- **Bounded waits.** A coordination request must be answered within 10 s
  (`BASEMIND_COMMS_REQUEST_TIMEOUT_SECS`), connect plus `Hello` within 30 s
  (`BASEMIND_COMMS_HANDSHAKE_TIMEOUT_SECS`), and the stdio relay answers a request the daemon never
  replied to within 180 s (`BASEMIND_RELAY_REQUEST_TIMEOUT_SECS`) with a retryable `-32002`.
  Forwarded work (scan, embed, memory, git-history and index reads) is exempt from the 10 s limit.
  The MCP `wait` mode is capped at 40 s and is cancel-aware.
- **Store off the reactor.** The daemon runs 8 async workers. Store reads use `spawn_blocking`;
  mutations additionally take a write gate so read-modify-write sequences stay serialized. On Unix
  the socket is accepted before the store opens and only `Ping` is answered until it does; other
  requests park, they do not fail.
- **Idempotent posts.** `agents` `post` takes an `idempotency_key`. The store dedupes on
  `(from, thread, key)` for one hour and persists the row in the same batch as the message.
- **Relay replay.** After a backend restart the relay re-sends an in-flight request once, only when
  that is safe: read methods, `code` / `git` / `graph`, idempotent `agents` modes, and `post`
  (stamped with a generated key when the caller gave none). Everything else gets a retryable
  `-32001 backend_restarted`.
- **Observability.** The detached daemon logs to `daemon.log`, rotated at 8 MiB.

## Consequences

- Client and daemon must speak the same protocol major; the first call after an upgrade replaces
  the old daemon, briefly.
- A keyed post cannot be re-sent with a different body under the same key within the hour; the
  original id wins.
- A request the relay cannot prove safe fails visibly rather than possibly running twice.
- The write gate serializes comms mutations; throughput is bounded by one writer, which matches the
  workload.

## Alternatives considered

- **Replay every request** - rejected: it can run a mutation twice.
- **Fail every in-flight request on restart** - rejected: it turns routine upgrades into visible
  errors for read-only traffic.
- **Server-side dedupe on message content** - rejected: two intentional identical posts are legal.
- **Unbounded waits with client-side cancellation only** - rejected: a wedged broker would hang
  tools indefinitely.
