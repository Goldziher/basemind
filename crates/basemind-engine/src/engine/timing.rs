//! Canonical contract for the `elapsed_us` field carried by the responses of `code_outline`,
//! `code_search`, `docs_search`, `git_blame`, the memory tools and `index_status`; `code_read`
//! answers with the source and no timing. The locating tools (`code_symbols`, `code_grep`, `code_refs`,
//! `find_files`, `git_log`, `git_diff`) leave it out: their answer is a few rows, the field cost as
//! much as one, and ADR-0031 holds them to the shell's token cost. Telemetry records the latency of
//! every call either way.
//!
//! basemind's index-backed queries are genuinely microsecond-scale (an indexed `commits_touching`
//! is ~37 µs against ~2 ms for the equivalent live git walk), so latency is reported in
//! **microseconds**. Millisecond granularity would round most of the hot path to `0`.
//!
//! # What `elapsed_us` measures
//!
//! The **tool body's own execution**. A monotonic [`Instant`] is taken as the first statement of the
//! tool body and read just before the response is serialized, so it covers exactly the work basemind
//! does to answer the call — index and store lookups, git walks, ranking, and building the response.
//!
//! "Tool body" means the `async` block inside the `#[tool]` shim when there is one, and otherwise the
//! first statement of the `run_*` helper it delegates to. The distinction matters: several shims
//! (`find_references`, `find_callers`, `workspace_grep`, `find_implementations`) await the in-RAM map and acquire the store lock *in the shim* before calling
//! their helper. Those shims start the timer themselves and pass the [`Instant`] down, so the wait and
//! the lock acquisition are inside the measured region rather than silently omitted from it.
//!
//! The timer is deliberately NOT the `__started` instant the shim hands to `record_call`: that one is
//! taken before the telemetry params snapshot, which is not part of answering the query.
//!
//! # What it excludes
//!
//! - MCP / JSON-RPC transport (the server never observes it).
//! - Deserialization of the tool's arguments, and the telemetry params snapshot taken by the shim.
//! - Serialization of the response itself (JSON or TOON encoding), which happens after the read.
//!
//! Excluding serialization is deliberate: it keeps the number comparable across `format: "json"`
//! and `format: "toon"`, and across result-set sizes, so it reports *query* cost rather than
//! *encoding* cost.
//!
//! # Cold-start caveat — the number is not identical on the first call
//!
//! Most read tools begin by awaiting the in-RAM code map (`await_cache_ready`), and the git tools
//! lazily build or load the git-history index on first use. Both of those waits happen **inside**
//! the measured region. So a first call issued against a cold server can be orders of magnitude
//! slower than the steady-state call that follows, and the two numbers mean different things.
//!
//! This is disclosed rather than hidden: when the server is still warming up or (re)building, the
//! response's `notice` field carries a [`LifecycleNotice`](crate::engine::types::LifecycleNotice) whose
//! `state` is `"warming_up"` / `"building_index"` / `"rescanning"`. **A caller benchmarking steady-state
//! query latency should discard any sample whose response carried a `notice`.**
//!
//! # Overhead
//!
//! Two [`Instant::now`] reads per call (tens of nanoseconds) at the tool boundary only. Nothing in
//! this module is called from a scanner or extraction inner loop.

use std::time::Instant;

/// Microseconds elapsed since `started`, saturating at [`u64::MAX`] rather than wrapping.
///
/// See the [module docs](self) for exactly what the resulting number does and does not include.
pub(crate) fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}
