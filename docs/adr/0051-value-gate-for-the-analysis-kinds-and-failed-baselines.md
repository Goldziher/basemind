---
status: proposed
date: 2026-11-19
amends: ["0031"]
---

# ADR-0051: Value gate for the analysis kinds, failed baselines and the binary's identity

- **Deciders:** the repository owner
- **Related:** amends ADR-0031; ADR-0022 (value gate), ADR-0047 (the analysis commands)

## Context

ADR-0031 gives every task kind a job and a pass rule, but lists only the kinds of 0.33. The 0.34
analysis kinds of ADR-0047 (`impact`, `cycles`, `grep_changed`, `find_large`, `read_around`,
`explain`) have task files and run in the gate, and the second 0.34 gate run had to classify them by
hand. That run also found:

- `benchmarks/eval/gate.py` listed none of the six kinds under a tool, so a failing analysis kind
  could not fail its tool in `gate.md`.
- The `cycles` baseline was every import line of the repository: 81,314 and 111,942 tokens, ratios
  of 1178x and 965x. It measures the repository's size, not the tool, and had no recall to compare.
  `explain` had no recall to compare either.
- A baseline that printed nothing counted in the median as a ratio of 0: the shell's failure read
  as the shell's win. A baseline that errored dropped out of the baseline's mean recall silently.
- `run.tsv` recorded only `basemind --version`, which is not bumped on main, so ADR-0031 rule 7 ("the
  binary is the build being released") could not be checked from the report.

## Decision

ADR-0031 stands except where this record adds to it.

1. **Jobs of the analysis kinds.**

   | Job | Kinds |
   |---|---|
   | Locating | `impact`, `grep_changed`, `find_large` |
   | Reducing | `read_around`, `explain` (as ADR-0047 calls it) |
   | Correctness | `cycles` |

2. **Correctness job.** A kind that no shell command an agent would run can answer has no baseline,
   and passes on credited at least 0.7 alone. Finding an import cycle means resolving every import to
   a file and walking the graph; no `git grep` or `git log` does that, and no agent reads every
   import line of a repository. `cycles` is the only such kind. A kind joins this job only through an
   ADR that says why no shell answer exists.
3. **Each kind counts against the tool it drives** (`EvalMode::tool_key`): `impact` and `cycles`
   against `code_refs`, `grep_changed` against `code_grep`, `find_large` against `find_files`,
   `read_around` against `code_read`, and `explain` against `code explain` (CLI-only, ADR-0047).
   `gate.py` refuses a gated kind that it lists under no tool, or under a tool other than the one the
   report names.
4. **`explain` baseline items.** The baseline is scored against the same gold: `path:line` for each
   definition and call-site row, and `->name` for each name a body line calls. Its recall is
   reported. As a reducing kind, `explain` gates on credited and ratio only.
5. **Failed baselines.** A baseline that errors or prints nothing is a baseline failure. Its recall
   is 0 wherever the kind reads baseline items, and it is left out of the ratio. The median ratio is
   taken over the credited tasks whose baseline answered. The baseline runs even when the tool
   errored, and the tool's and the baseline's mean recall are taken over the same tasks. The report
   counts failures per kind (`baseline_failed`).
6. **Undecidable checks.** These cases get no verdict rather than a pass: a locating kind with no
   baseline recall to compare, a kind whose credited tasks all have failed baselines, and `grep`
   without a measured symbol accuracy.
7. **Binary identity.** The report records the Cargo version, a hash of the executable and the
   features the binary was compiled with. `run_gate.sh` records the commit of the checkout it runs
   from (with `-dirty` for uncommitted tracked changes), and `gate.md` prints all of them.

## Consequences

- On the second 0.34 run's numbers, more tools fail. `code_refs` also fails on `impact` (basemind
  0.58x), `code_grep` on `grep_changed` (basemind 0.60x), `find_files` on `find_large` (0.44x,
  0.50x) and `code_read` on `read_around` (1.25x, 1.33x). `code_read` passed before only because no
  analysis kind counted against it. `cycles` passes on credited alone.
- Rule 5 changes no verdict on that run. It drops 11 tasks from the ratio, and tokio `impact` moves
  from 0.78x to 0.87x.
- The committed task files change only in the `cycles` and `explain` baselines.
- `run_gate.sh` must run from the checkout the binary was built from. The recorded commit is that
  checkout's, not one the binary carries; the executable hash ties the report to the bytes that ran.

## Alternatives considered

- **A `cycles` baseline from the importers of the file** (`git grep -l` for `use` lines naming it):
  rejected. It returns the files that import the target, not the cycle. For a cycle of more than two
  files, its recall would measure the grep, not what an agent concludes.
- **Keep the import-line dump as a tokens-only baseline for `cycles`**: rejected. Its ratio cannot
  fail, and it measures repository size.
- **Charge an empty baseline as a 0x ratio**: rejected, because it counts the shell's failure as the
  shell's win.
- **Drop a failed baseline from the recall mean**: rejected, because it hides the failure and makes
  the baseline look better than it is.
- **Gate recall parity on reducing kinds now that `explain` has items**: deferred. ADR-0031 gates
  parity for locating kinds only, and a new threshold needs its own record.
