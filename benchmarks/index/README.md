# Index read baseline

Read latency of the Fjall index, recorded on the layout where every git worktree owns a full index.
The move to one content-keyed index per repository group is gated on these numbers.

## What is measured

`benches/index_read.rs` builds a synthetic repository group: a git repository of 900 source files
(Rust, TypeScript and Python in equal parts, 8 functions and 40 call sites per file, 25% of call
sites spread over five hot callee names) plus linked worktrees that each change 10% of the files.
It indexes every worktree with `basemind::scanner::scan` and times these reads over `IndexView`:

| operation | read |
|---|---|
| `references/*` | `calls_by_callee` name scan, first page of 100, plus the `calls_by_path` lookup per hit |
| `callers/*` | the exact name scan, then `references_to` and the call sites of every file holding a use |
| `implementations/*` | `implementations_by_trait` name scan, first page |
| `symbol_exact/*` | `symbols_by_name_lookup`, the code-search exact lane |
| `grep_bloom/*` | `grep_skip_verdicts` over every file of the worktree |

`hot`, `warm` and `cold` say how many rows carry the name. Each operation runs at 1, 3 and 10
content versions: the same paths indexed in that many worktrees. A sample is one read against one
worktree, round-robin over the group.

Before timing, the benchmark checks every worktree's index against what the generator wrote (call
rows per name, implementations per trait, definitions per name, bloom verdicts) and panics on a
mismatch, so a read that returns nothing cannot be timed as a fast read.

BM25 keyword search is not covered: `basemind::search` and the posting writer exist only under the
`code-search` feature, and this baseline is a default-feature build.

## Run it

```sh
cargo bench --bench index_read -- --out benchmarks/index/baseline-<date>.json
cargo bench --bench index_read -- --quick     # 300 files, 200 samples
```

The benchmark points `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR` at its own temp directory and
never starts a `basemind` binary.

## Baseline, 2026-11-16

Apple M4 Pro (10 P + 4 E cores, 48 GB), macOS 27.2.1, rustc 1.100.0, default features, commit
`46cf7933`. Microseconds, 2000 samples per cell. Each cell is `run 1 / run 2`: the same binary run
twice in a row (`baseline-2026-11-16.json`, `baseline-2026-11-16-run2.json`).

| operation | 1x p50 | 1x p95 | 3x p50 | 3x p95 | 10x p50 | 10x p95 | rows/read |
|---|---|---|---|---|---|---|---|
| `references/substring/hot` | 311.8 / 273.4 | 744.9 / 6710.0 | 353.2 / 303.9 | 1857.8 / 2964.6 | 373.9 / 379.3 | 463.5 / 454.3 | 2100 |
| `references/exact/hot` | 317.5 / 280.0 | 899.2 / 7254.0 | 344.5 / 304.0 | 1217.7 / 2243.2 | 395.5 / 376.6 | 484.0 / 1171.5 | 2100 |
| `references/exact/warm` | 127.8 / 114.0 | 376.4 / 913.4 | 119.2 / 122.2 | 364.4 / 567.5 | 151.1 / 149.1 | 469.5 / 186.2 | 662 |
| `references/exact/cold` | 6.8 / 5.1 | 17.3 / 16.3 | 6.3 / 6.8 | 17.8 / 21.3 | 7.4 / 7.6 | 9.9 / 10.8 | 6 |
| `callers/hot` | 340.9 / 300.8 | 946.6 / 7871.3 | 393.2 / 335.1 | 1176.6 / 1723.8 | 508.9 / 510.2 | 663.2 / 1407.8 | 2100 |
| `callers/cold` | 6.2 / 8.4 | 18.0 / 41.0 | 8.0 / 6.3 | 33.0 / 17.6 | 9.4 / 10.1 | 24.0 / 46.2 | 20 |
| `implementations/hot` | 163.7 / 143.9 | 170.9 / 567.4 | 153.8 / 155.8 | 463.5 / 596.7 | 203.4 / 184.3 | 331.1 / 272.6 | 1125 |
| `implementations/warm` | 149.3 / 132.6 | 156.7 / 399.9 | 136.3 / 143.8 | 261.5 / 503.4 | 192.2 / 173.9 | 538.3 / 474.0 | 1125 |
| `symbol_exact/hot` | 14.9 / 12.0 | 15.3 / 13.0 | 12.3 / 13.8 | 13.0 / 14.2 | 13.8 / 14.5 | 14.2 / 33.8 | 100 |
| `symbol_exact/cold` | 0.7 / 0.6 | 0.8 / 0.7 | 0.6 / 0.6 | 0.7 / 0.8 | 0.7 / 0.7 | 1.1 / 1.1 | 1 |
| `grep_bloom/rare` | 3884.5 / 3009.2 | 17966.6 / 40838.9 | 2064.0 / 2600.7 | 9388.0 / 31384.8 | 5957.2 / 1649.9 | 43703.5 / 13614.1 | 903 |
| `grep_bloom/absent` | 4108.9 / 2671.0 | 27957.2 / 38574.8 | 2861.6 / 2624.1 | 9784.2 / 17666.9 | 4236.1 / 1567.2 | 42161.6 / 2622.9 | 903 |

**These runs were taken on a loaded machine** (load average 46 to 69 on 14 cores, other builds
running). The two runs disagree by up to 25% at p50 and by up to 8x at p95, so the p95 columns
are not a usable reference and no 10% comparison can be made against this file. Record the
baseline again on an idle machine before stage 4 is judged; the procedure below does not depend
on the absolute values here.

What grows with the version count, from run 1:

| versions | paths per worktree | distinct contents | index on disk | `get` rows: one index / group sum / by distinct content |
|---|---|---|---|---|
| 1 | 903 | 903 | 19.9 MiB | 5580 / 5580 / 5580 |
| 3 | 903 | 1086 | 59.8 MiB | 5580 / 16923 / 6954 |
| 10 | 903 | 1710 | 199.6 MiB | 5580 / 56607 / 11459 |

On today's layout a read touches one worktree's index, so rows per read are the same at 1x, 3x
and 10x (2100 for a hot name: the scan cap of 2000 matches plus 100 position lookups). Disk and
total rows grow linearly with worktrees. p50 of the hot scans rises by 20 to 50% from 1x to 10x in
both runs (ten open databases instead of one), which is within what this machine's load can
explain. In a content-keyed group index a name scan walks the rows of every content version of the
name, the last column above, and drops those not live in the worktree: at 10x that is 2.05 times
the rows of one index for `get`. That is the cost the gate bounds, and `rows/read` will show it
directly.

## The gate

Stage 4 of the redesign (re-keying a row family by content) must keep p95 at 3 content versions
within 10% of the per-worktree layout, for every operation.

1. On an idle machine, build the bench at the commit before the change and at the change.
2. Run before, after, before. The two "before" runs give the noise floor per operation.
3. An operation whose two "before" p95 values differ by more than 5% is not judged; rerun it with
   more samples (`--samples`).
4. Fail if `after` p95 at 3x exceeds the larger "before" p95 by more than 10% for any judged
   operation. Report `rows/read` next to it: a regression with unchanged rows is not the layout's.

The workload (corpus, queries, sample order) lives in `benches/index_read.rs` and
`benches/index_read/corpus.rs` and must not change between the two sides. Only
`benches/index_read/ops.rs` knows the layout.

## Cost of an added worktree

`tests/harden.rs` has a multi-worktree mode (`tests/harden/worktrees.rs`). For one canary clone it
adds K git worktrees at older first-parent commits, scans each, and records bytes on disk by
holder, the time to open and first-scan the worktree, and the resolve pass's share of the scan.

```sh
BASEMIND_HARDEN_FEATURES="" BASEMIND_HARDEN_SKIP_SUITE=1 BASEMIND_HARDEN_WORKTREES=5 \
  ./scripts/harden.sh ripgrep
```

`BASEMIND_HARDEN_WORKTREE_STEP` sets the commits between worktrees (default 50), and
`BASEMIND_NO_SEED=1` measures a cold first scan instead of one seeded from a sibling's index. The
table is appended to `<harden root>/worktrees.md`, the rows to `worktrees.ndjson`.

ripgrep, 2026-11-16, default features, sibling seeding on, same machine and load as above
(`harden-worktrees-2026-11-16.md`): an added worktree costs 9.0 MB, of which 8.8 MB is journal,
168 KB keyspaces, 28 KB file map and nothing in vectors (no vector feature in this build). The
times in that table (open 675 ms, first scan 6.2 s for about 220 files) were taken under the same
load and are far above an idle machine's; only the byte columns are a usable reference.

The resolve share is timed from outside the crate by polling the scan's phase breadcrumb every
0.5 ms, because the scan report carries no phase timings.
