---
status: accepted
date: 2026-11-23
amends: ["0057"]
---

# ADR-0058: `code impact`: buckets, order of work and `--fail-above`

- **Deciders:** the repository owner
- **Related:** amends ADR-0057 (callers and downstream), ADR-0047 (the analysis commands; exit
  checks, §6)

## Context

`code impact` lists what a change reaches but not how much of it matters, where to start, or whether
it is too much for CI to accept.

## Decision

1. **Buckets.** Every impacted file is `prod`, `config`, `test` or `external`, judged from its path
   alone, first match wins in the order external, test, config, prod. External is a vendored or
   third-party directory (`vendor`, `node_modules`, `third_party`, `site-packages`, `.venv`) or a file
   indexed from outside the root. Test is a test directory (`test`, `tests`, `__tests__`, `spec`,
   `e2e`, `fixtures`, `testdata`) or a test file name (`test_x`, `x_test`, `x.test`, `x.spec`,
   `FooTest`, `conftest`). Config is a configuration extension (`toml`, `yaml`, `json`, `ini`, ...),
   a build or tool file (`build.rs`, `Dockerfile`, `Makefile`), a `*.config.*` file or a config or
   automation directory (`.github`, `config`, `ci`). Anything else is prod.
2. **Impacted files.** The importers reached plus the files holding a caller, each counted once,
   targets excluded: `impacted_files`, with the per-bucket counts in `buckets`.
3. **Order of work.** `order` lists the targets first (depth 0), then the impacted files by the
   fewest import or call hops, and within a depth by bucket (prod, config, test, external) and path:
   change the code a change reaches first, its tests after, code the repository does not own last.
4. **Gate.** `--fail-above <n>` exits 1 when `impacted_files` exceeds `n`, through the exit check
   of ADR-0047. A report from an index that is not ready exits 1 saying so, as `--fail-on-cycle`
   does. A walk cut short (`truncated`) undercounts, so the gate can only pass too much, never fail
   too little.

## Consequences

- The classification is a path heuristic: a test helper outside a test directory or a config file
  with a code extension is bucketed `prod`.
- The fields are added to the `code impact` answer only; no tool schema changes.

## Alternatives considered

- **Per-row bucket on `affected` and `callers`** — rejected: `order` already names every impacted
  file once with its bucket, and the rows stay as ADR-0057 defines them.
- **Fail on a weighted score** — rejected: a plain file count is predictable in CI.
