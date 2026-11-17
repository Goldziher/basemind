---
status: accepted
date: 2026-11-17
amends: ["0022"]
---

# ADR-0031: Value gate by job — locating and reducing are judged differently

- **Deciders:** the repository owner
- **Related:** amends ADR-0022 (value gate); ADR-0018, ADR-0027

## Context

ADR-0022 set one pass rule for every task kind: at least 0.7 of the tasks credited, and a median
ratio of baseline tokens to tool tokens of at least 1.5. The gate ran for the first time on
16 November 2026 (`benchmarks/eval/results/gate.md`). Only `code_read` passed every kind.

The run showed that the rule cannot be met by a whole class of tools, and that ADR-0022 leaves
several things undefined:

- A lookup whose answer is a list of places (a definition, a call site, a matching line, a file, a
  commit) has a shell answer that is already minimal: `git grep -n` prints `path:line:text`.
  No tool can answer in two thirds of those tokens. `code_grep` matched the baseline's recall on
  every task and still measured 0.30x and 0.35x.
- ADR-0022 defines the baseline as "a targeted `git grep` plus up to three whole-file reads". That
  says nothing for `git_log`, `git_blame`, `git_diff` or `find_files`, and does not say when the
  file reads happen. The run read whole files only where the answer is file content.
- The credited fraction's denominator, the median's definition, recall on ranked results and the
  cost of a paged answer are not stated.
- The corpus is "basemind at the release tag", and the binary under test is not named.
- `code_search` and `docs_search` need an embedding model, which the run could not download.

## Decision

The value gate of ADR-0022 stands except where this record changes it.

1. **Two jobs.** Every task kind is either a locating kind or a reducing kind.

   | Job | Kinds |
   |---|---|
   | Locating: the answer is a list of places | `symbols`, `grep`, `references`, `callers`, `implementations`, `dependents`, `find`, `git_search`, `git_path`, `git_symbol`, `diff` |
   | Reducing: the answer replaces reading content | `outline`, `read`, `blame`, `blame_symbol`, `diff_structural`, `docs`, `semantic` |

2. **Pass rule for a reducing kind.** Unchanged: credited at least 0.7 and median ratio at least
   1.5, on each corpus.
3. **Pass rule for a locating kind.** On each corpus, all of:
   - credited at least 0.7;
   - mean recall no more than 0.02 below the baseline's mean recall;
   - median ratio at least 0.67 over the credited tasks, that is, the tool's answer costs at most
     one and a half times the shell's.
4. **What a locating tool adds is measured where it can be.** `code_grep` reports the enclosing
   symbol of each hit; its accuracy against the gold must be at least 0.95. This is the only such
   measure today; a tool that claims another gains a measure with it.
5. **Baselines, per kind.** The baseline is the shell command an agent would run, with no
   whole-file read unless the answer is file content:

   | Kinds | Baseline |
   |---|---|
   | `symbols`, `grep`, `references`, `callers`, `implementations`, `dependents` | `git grep -n` with a targeted pattern |
   | `outline`, `read` | the whole file |
   | `find` | `git ls-files` filtered by the query |
   | `git_search`, `git_path`, `git_symbol` | `git log` with the matching option |
   | `blame`, `blame_symbol` | `git blame` over the lines |
   | `diff`, `diff_structural` | `git diff` between the two revisions |
   | `docs`, `semantic` | `git grep -n` on the query terms plus up to three whole-file reads |

6. **Counting rules.**
   - The credited fraction is over every task of the kind, including tasks the tool answered with
     an error.
   - The median is the nearest-rank median.
   - Recall on a ranked answer is recall within the returned results.
   - A paged answer is charged for every page up to the absent `next_cursor`.
7. **Corpus and binary.** The basemind corpus is the previous release's tag; the binary is the build
   being released, built with the `tokenizer` feature. Both pins are in `benchmarks/eval/corpus.tsv`.
8. **Tools that need a model.** `code_search` (kind `semantic`) and `docs_search` are run with their
   models present, on a machine that has them, before the release is cut. A release whose report
   lists either as "not run" has not run the gate.
9. **`code_refs`** must pass on `references`, `callers`, `implementations` and `dependents`.
10. **Gold.** The Rust gold comes from the committed `syn` extractor
    (`benchmarks/eval/rust_gold/`) and is name-based: it does not resolve scope. Precision against it
    is reported and does not gate.

## Consequences

- On the first run's numbers the new rule still fails `code_symbols` (0.29x, 0.32x), `code_grep`
  (0.30x, 0.35x), `find_files` (0.19x, 0.21x), all three `git_log` kinds (0.11x to 0.25x),
  `code_refs` on `implementations` (credited 0.36), on `callers` and `references` (recall 0.12 or
  more below the baseline on both corpora) and on `dependents` (0.50x, 0.55x), `code_outline` on tokio (credited 0.68) and
  `git_blame` by symbol on tokio (1.11x). `git_diff` passes (1.04x, 1.00x with recall at parity).
  The work the gate asks for is therefore unchanged in kind: leaner rows, and the extraction and
  history defects the run found.
- The 0.67 floor is a judgement, not a measurement. It allows a locating tool a fixed overhead per
  answer and no more; it is revisited with the second report.
- A locating tool that only matches the shell passes. What justifies it is that it costs no more
  than the shell while sharing an index, paging and a refusal contract with the tools that reduce.
  ADR-0022 rejected a threshold of 1.0 for that reason; this record accepts 0.67 for locating kinds
  because the alternative removes every locating tool regardless of quality.
- Rule 8 means the gate cannot complete in a sandbox without network access.
- The consecutive-failure rule of ADR-0022 counts from the 0.34.0 report, judged by this rule.

## Alternatives considered

- **Keep 1.5 for every kind** — rejected: no locating tool can pass, so the gate would measure the
  job, not the tool.
- **Let the baseline read up to three files for every lookup** — rejected: an agent that greps for a
  definition does not then read three files, and the 27.9x recorded for `code_symbols` came from
  that assumption.
- **Gate locating kinds on recall alone** — rejected: `git_log` by path costs four to ten times the
  shell's answer, and nothing else in the gate would notice.
- **Gate on precision as well** — deferred: the gold is name-based, so precision against it punishes
  correct scope resolution. Reported, not gating.
