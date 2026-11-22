---
status: accepted
date: 2026-11-22
amends: ["0031", "0051"]
---

# ADR-0055: A 1.4x reducing floor for `read_around`

- **Deciders:** the repository owner
- **Related:** amends ADR-0031 (value gate by job; its reducing floor of 1.5x) and ADR-0051 (which
  classifies `read_around` as reducing); ADR-0052 (rows as the shell prints them), ADR-0053 (text
  answers encoded per tool)

## Context

`read_around` is `code_grep` with `context: 5` against the shell's `rg -n -C5`, a reducing kind with
a median baseline/tool token floor of 1.5x. On the 0.34.0 value gate run on the shipped build it
measured a median of 1.4295x on basemind (1.61x on tokio, which passes). ADR-0053 already took the
encodings down to the rows `rg` prints plus the `# <path>` and `## <symbol>` headers, and named
those headers as what remains: the enclosing symbol is a fact `rg` does not give, and ADR-0052 and
ADR-0053 keep it on purpose. Closing the last 0.07x means dropping the headers, which trades the
answer for the metric.

## Decision

`read_around` is gated at a median ratio of 1.4x; the credited floor of 0.7 and the recall rule are
unchanged. Every other reducing kind keeps 1.5x, and the locating and correctness jobs are
untouched. The floor is `READ_AROUND_MIN_RATIO` in `src/eval/gate.rs`, and `gate.md` prints it.

## Consequences

- The kind passes on both corpora (1.43x / 1.61x) and a regression below 1.4x still fails the gate.
- ADR-0031's rule 2 now reads 1.5x for every reducing kind but this one; a further kind needs its own
  record to take a different floor.
- `benchmarks/eval/results/` carries the verdict of the run it recorded; see the README for how the
  committed verdict relates to this floor.

## Alternatives considered

- **Drop the symbol headers** — rejected by ADR-0053: the enclosing symbol is what `code_grep` adds
  over `rg`.
- **Lower the floor of every reducing kind to 1.4x** — rejected: the other kinds pass 1.5x with
  room, and the lower floor would let them regress unnoticed.
- **Leave the gate failing** — rejected by the owner: the shortfall is the intended row design, not a
  defect.
