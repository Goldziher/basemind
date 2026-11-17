# Pre-release hardening

Four scripts under `scripts/hardening/` prove a release build has no memory, CPU, descriptor,
thread or process leaks, and that every MCP tool and its CLI command works, document RAG included.
They need Python 3.10+ and nothing outside the standard library.

| Script | Proves | Typical duration |
|---|---|---|
| `soak.py` + `analyze.py` | no growth in memory / fds / threads under load, quiet idle, clean shutdown, bounded data dir | load + idle + ~1 min (15 min for a short run, 1 h+ for a release) |
| `functional_sweep.py` | every verb tool and its CLI command answers with the expected structure; RAG finds planted facts, with filters | 1-3 min (`comms` build); 5-15 min first time on a `full` build (model downloads) |
| `leak_check.py` | no definite leaks in one-shot `basemind scan` (macOS `leaks`) | 1-2 min for the fixture; longer with `--repo` |
| `selftest_analyze.py` | `analyze.py`'s rules fire on known-bad input and pass known-good input | seconds |

Build first, outside the repository's `target/` if you like:

```bash
cargo build --release --features comms        # the daemon + serve relay, no RAG
cargo build --release --features full          # adds documents, code_search, memory (heavy)
```

## Isolation and safety

Every basemind process the scripts start gets a fresh `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR`
under a new temp directory, every inherited `BASEMIND_*` variable is dropped (so
`BASEMIND_NO_AUTOSPAWN` is never set), and the scripts refuse to start a process whose data home or
comms dir is outside that temp directory or is the real per-user data home.

A process belongs to a run when its working directory, or (for a `basemind` binary) one of its
open files, lies inside the run's temp directory, or when it descends from such a process. The
daemon that `serve` auto-spawns is detached (re-parented to init), so parentage cannot find it, but
its cwd is the run's comms dir and it holds the run's `daemon.lock`; `serve` and CLI one-shots run
with the repo copy as cwd. (macOS does not expose another process's environment, so an environment
marker cannot do this job.) On exit, Ctrl-C, SIGTERM or an exception the scripts SIGTERM, then
SIGKILL, every process of their own run, re-checking ownership right before each signal. Another
session's process has nothing inside a fresh `mkdtemp` directory, so other basemind daemons on the
machine are never touched.

Raw output goes to `<polyrepo root>/reports/hardening/<kind>-<timestamp>/` (untracked) unless `--out`
says otherwise; outside the polyrepo it falls back to the system temp dir.

## Soak

```bash
python3 scripts/hardening/soak.py --binary "$CARGO_TARGET_DIR/release/basemind" \
    --duration 600 --warmup 120 --idle 180
python3 scripts/hardening/analyze.py reports/hardening/soak-<ts>/soak.ndjson
```

`--repo PATH` soaks a copy of another repository (default: `git clone --local` of this checkout's
HEAD into the temp dir; the original is never written). For a release, run at least
`--duration 3600 --idle 600`.

What it does:

1. A direct CLI `basemind scan` of the copy (no daemon yet).
2. Opens the long-lived `main` MCP session (`basemind serve`, which auto-spawns the daemon) and picks
   targets from the index: a code file with two functions, one of them, a trait/interface/class, an
   imported module.
3. Loops for `--duration` seconds. Each cycle: every verb tool with realistic arguments (28-30 calls,
   including `code_search`, `docs_search`, `memory_save` / `memory_search`; a build without their
   feature answers "not compiled" and the tool is recorded once as skipped); an edit cycle that
   rewrites one file, adds one and deletes another under `hardening_soak/`, then `index_rescan` of
   those paths and a check that the new symbol is found and the deleted file's is gone; every
   `--full-scan-every` cycles a whole-workspace `index_rescan` and a CLI `basemind rescan` (forwarded
   to the daemon; a CLI `scan` refuses with exit 3 while a daemon runs); `--sessions-per-cycle` short sessions opened and closed, alternately by stdin EOF
   and by SIGKILL; and CLI one-shots (`status`, `code symbols`, `git log`, `code grep`). Edit contents
   repeat every `--content-period` cycles, so the set of distinct contents is fixed and a correct
   store stops growing once it has seen them all.
4. Idle for `--idle` seconds: the `main` session stays connected, no calls.
5. Shutdown: close `main` by stdin EOF; start a throwaway MCP host process that runs `serve`, then
   SIGKILL the host (serve must exit on EOF); start a session and SIGKILL `serve` itself (the
   daemon must release that connection's descriptors); `basemind comms stop` (the daemon must
   exit); then a sweep for any process of the run still alive.

Every `--sample-every` seconds (default 5) a sampler thread records, for each process of the run
(daemon, serve, CLI one-shots, any child): RSS, physical footprint (macOS), virtual size, thread
count, open fd count, cumulative CPU time and its delta, and idle wakeups (macOS); plus the data
dir and comms dir sizes and a census of live processes. macOS reads `proc_pid_rusage` /
`proc_pidinfo` through `ctypes`; Linux reads `/proc`.

### Pass rules (`analyze.py`)

"Long-lived" means the daemon and the `main` serve. Slopes are least-squares fits over the `load`
phase only (the first `--warmup` seconds of load are excluded), in units per minute.

| Rule | Fails when | Default threshold | Why this number |
|---|---|---|---|
| R0 | fewer than 20 post-warm-up samples for every long-lived process, fewer than 10 idle samples, no tool calls | 20 / 10 | a run that measured nothing must not pass |
| R1 | more than one daemon pid seen before shutdown | 1 | a replaced daemon is a crash or a takeover; it also resets every slope |
| R2 memory | slope > 1 MiB/min **and** slope − 2·SE > 0 **and not** (second-half slope + 2·SE < 1 MiB/min) | `--mem-tol 1.0` | 1 MiB/min is 1.4 GiB/day for an always-on daemon: the smallest growth that would matter in practice and still clears sampling noise over a 10-minute window. The significance and plateau clauses let a cache that fills after warm-up and then levels off pass, while a noisy but steady climb still fails. On macOS the gate is the physical footprint (what Activity Monitor and jetsam count); RSS is judged too |
| R3 fds | same shape | `--fd-tol 0.2` fds/min | one descriptor lost per five minutes; a per-call or per-session leak under this workload is several per minute |
| R4 threads | same shape | `--thread-tol 0.2` threads/min | as fds: a thread per session or per rescan shows as several per minute |
| R5 | max live processes of the run in the second half of load > max in the first half | 0 extra | sessions and one-shots are sequential, so the census is flat unless something accumulates |
| R5b | a closed session's `serve` did not exit, or is still sampled more than 15 s after it was closed | `--exit-grace 15` | |
| R6 | mean CPU of all the run's processes over the idle window (after a 30 s settle) | `--idle-cpu 0.01` (1 % of one core) | an event-driven daemon with connected but silent clients should be ~0; a busy-wait or a tight poll loop is several percent to 100 % |
| R6b (macOS) | idle wakeups/s summed over the run's processes | `--idle-wakeups 100` | 100/s is a 10 ms timer: catches a poll loop too cheap to show as CPU |
| R7 | any shutdown step whose process did not exit within the grace; the daemon holding more than 2 fds above its pre-session count after its serve was SIGKILLed; any process of the run alive after the final step | 10 s grace, `--fd-slack 2` | |
| R8 | data dir slope over load, same shape as R2 | `--dir-tol 1.0` MiB/min | contents repeat, so after one period everything is already stored; anything still climbing is unbounded growth (logs, unreclaimed blobs, LSM garbage) |
| R9 | any transport error (no answer, dead server), any `isError` on a call expected to succeed, an edit not visible after `index_rescan`, a CLI one-shot exiting non-zero | 0 | the workload must have done what it claims |

Every threshold is a flag on `analyze.py`; a rerun with a different value must say so.

### Reading a failure

The report (`soak.report.md` beside the NDJSON) lists each rule with the measurement it judged,
then per-process start/max/end tables for warm-up, load and idle, and per-tool call counts and
latencies.

- **R2/R3/R4 fail:** the detail shows the slope, its 2·SE band and the second-half slope. Plot the
  series to see whether it is linear (leak) or a late step (a cache filling: rerun with a longer
  `--warmup`; if it still fails, the cache is unbounded). For fds on macOS, `lsof -p <pid>` on a
  kept run (`--keep-temp`) shows what accumulates.
- **R6 fails:** the detail names the busiest process. `sample <pid> 5` (macOS) or
  `perf top -p <pid>` (Linux) during the idle phase shows the spinning stack.
- **R7 fails:** the detail lists survivor pids and argv. A `child:` role is a process basemind
  spawned and never reaped.
- **R9 fails:** the first failing call and its arguments are in the detail; all failures are `call`
  records with `ok: false` in the NDJSON.

`selftest_analyze.py` writes synthetic NDJSON for a healthy run, a cache that levels off after
warm-up, a 3 MiB/min climb, a busy idle, an fd leak, a leaked child and an empty run, and checks
that `analyze.py` passes the first two and fails each of the others on exactly the rule it targets.
`soak.py --inject-orphan` leaks a detached `sleep` (cwd: the run's temp root) before the survivor sweep, so a
real run proves R7 fires.

## Functional sweep

```bash
python3 scripts/hardening/functional_sweep.py --binary "$CARGO_TARGET_DIR/release/basemind"
```

Builds a fixture repository (Rust + Python, two commits, an uncommitted edit) with five documents,
each carrying one invented fact: Markdown, HTML, a hand-written text-layer PDF, an `.eml` and an
`.ipynb`. It scans, waits (up to `--docs-timeout`) for documents to become searchable, then makes
one call per case over MCP and runs the matching CLI command with `--json`, asserting structural
fields: the outline lists `compute_total` with a 1-based line, `code_refs` finds callers,
implementations, importers and a definition, `git_log` / `git_blame` / `git_diff` show the fixture's
history and edit, `memory_*` round-trips a note. For each planted question `docs_search` must return
the planted document among the top five with the answer in its text; a `mime_type` filter must keep
the PDF fact and an excluding filter must drop it.

A case answering that its Cargo feature is not compiled in is a counted skip. The run fails when any
case fails, when nothing passed, or when the server advertises a verb tool the sweep did not call.

## One-shot leak check (macOS)

```bash
python3 scripts/hardening/leak_check.py --binary "$CARGO_TARGET_DIR/release/basemind" [--repo .]
```

Runs `leaks --atExit -- basemind scan` with `MallocStackLogging=1` on the fixture (code and, on a
`documents` build, the documents) and optionally a clone of `--repo`. `--cmd status` adds more
one-shots. It fails on any definite leak and on a run `leaks` could not inspect. The full `leaks`
output, with stacks, is kept per run.

Linux equivalents (not automated here):

```bash
valgrind --leak-check=full --errors-for-leak-kinds=definite --error-exitcode=1 basemind scan
heaptrack basemind scan && heaptrack_print heaptrack.basemind.*.zst | grep -A20 'MEMORY LEAKS'
```

Run either with `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR` pointing at fresh temp dirs.

## Not covered

- Linux and Windows: the sampler supports Linux `/proc` but the scripts have only been run on macOS.
  Windows has no sampler.
- The daemon's own idle reap (10 minutes after the last client) is not waited for; `comms stop` is
  the shutdown exercised.
