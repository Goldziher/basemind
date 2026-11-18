# Architecture Decision Records

This directory holds basemind's Architecture Decision Records (ADRs): short documents that capture
a single significant architectural decision, the context that forced it, and its consequences.

## Why ADRs

basemind is built across many sessions by both humans and agents. An ADR makes each load-bearing
decision — and the alternatives we rejected — durable and greppable, so later work builds on the
decision instead of re-litigating it. They complement [`../ARCHITECTURE.md`](../ARCHITECTURE.md)
(which describes the system as it is now) by recording *why* it is that way.

## Format

Each ADR is one file, `NNNN-<kebab-slug>.md`, numbered in creation order from `0000` with no gaps.
Copy [`0000-template.md`](0000-template.md) to start a new one. The template follows the
Nygard/MADR shape:

- **Front matter** — a YAML block at the top of the file holding the record's metadata:

  ```yaml
  ---
  status: proposed
  date: 2026-11-15
  ---
  ```

  | Key | Value |
  |---|---|
  | `status` | `proposed`, `accepted`, `rejected`, `superseded`, `deprecated` or `withdrawn`, in lower case |
  | `date` | the day the record was written, `YYYY-MM-DD` |
  | `superseded-by` | on a superseded record, the successor's number as a quoted string: `"0017"` |
  | `supersedes` | on the successor, the number it replaces (`"0011"`), or a list of them |
  | `amends` | on an amending record, a list of the numbers it amends: `["0018"]` |

- **Title** — `# ADR-NNNN: <decision>`
- **Deciders**, **Related** — a bullet each under the title
- **Context** — the forces at play: the problem, constraints, and what exists today
- **Decision** — what we will do (active voice, present tense)
- **Consequences** — what becomes easier and harder as a result; follow-on work
- **Alternatives considered** — the options we rejected and why

ADRs 0000–0020 predate the front matter and also carry `- **Status:**` and `- **Date:**` bullets
under the title, some with a note on when and why the status changed. The front matter is the
authoritative value; where a record has both, they must agree.

## Lifecycle

1. Author the ADR as `status: proposed` and add its row to the index below.
2. A maintainer (human) reviews and moves it to `status: accepted` (or requests changes).
3. Implement against the accepted decision. An accepted record's decision is never edited. A later
   ADR overrides it by superseding or amending it:
   - **Supersede.** The new record carries `supersedes`; the old one gets `status: superseded` and
     `superseded-by`, and nothing else in it changes.
   - **Amend.** The new record carries `amends`; the old one stays `accepted`.

Keep ADRs short — a decision, not a design doc. Link related ADRs by number.

## Linting

[adrlint](https://github.com/Goldziher/adrlint) checks the log. It runs inside poly, switched on by
`[lint.markdown.adrlint]` in `poly.toml`, so there is nothing separate to install:

```bash
poly lint docs/adr
```

- Per record, also in the pre-commit hook: a title and a `## Context` section, no empty section,
  one level-1 heading, valid YAML front matter and working in-document anchors.
- Over the whole log, on a whole-tree run (`poly lint .`, which is what CI runs): it starts at
  `0000`, has no gap and no duplicate number, and holds only `NNNN-<slug>.md` files plus this index.

The MADR section names (`Decision Outcome`, `Considered Options`) are not required: the records
keep `Decision` and `Alternatives considered`. adrlint does not check the `status` vocabulary, so
keep to the values in the table above.

## Index

ADRs 0001–0010 form one roadmap: give basemind a knowledge-graph capability layer
(typed graph, traversal, communities), a rendering engine, an interactive UI, and document/rationale
graph edges. They are sequenced by dependency — foundations first, then the capabilities built on
them.

| ADR | Title | Status |
|---|---|---|
| [0000](0000-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0001](0001-unified-typed-code-graph.md) | Unified typed code-graph model | Superseded by ADR-0017 |
| [0002](0002-edge-provenance-and-confidence.md) | Edge provenance and confidence | Superseded by ADR-0017 |
| [0003](0003-graph-traversal-tools.md) | Graph traversal capabilities | Superseded by ADR-0017 |
| [0004](0004-community-detection.md) | Community detection and deterministic labels | Superseded by ADR-0017 |
| [0005](0005-rendering-engine.md) | Rendering engine — one payload, pluggable renderers | Superseded by ADR-0017 |
| [0006](0006-interactive-ui-tauri.md) | Interactive UI — Tauri desktop app | Withdrawn |
| [0007](0007-agent-launchable-display-tool.md) | Agent-launchable UI display tool | Superseded by ADR-0017 |
| [0008](0008-documents-code-graph.md) | Documents ↔ code graph | Superseded by ADR-0017 |
| [0009](0009-rationale-decision-nodes.md) | Rationale / decision nodes | Superseded by ADR-0017 |
| [0010](0010-branch-integration-release-strategy.md) | Branch, integration & release strategy | Superseded by ADR-0026 |
| [0011](0011-mcp-tool-surface-redesign.md) | MCP tool-surface redesign — consolidation into nine domains | Superseded by ADR-0018 |
| [0012](0012-grep-content-prefilter.md) | Per-file trigram bloom prefilter for `code grep` | Accepted |
| [0013](0013-comms-robustness-idempotency-replay.md) | Comms robustness: correlated requests, bounded waits, idempotent posts, relay replay | Accepted (amended by ADR-0021, ADR-0024, ADR-0030) |
| [0014](0014-cli-mcp-parity-guard.md) | Executable CLI/MCP parity guard | Accepted (amended by ADR-0018, ADR-0023) |
| [0015](0015-bounded-document-extraction.md) | Bound document extraction with a thread cap, tombstones and a phase budget | Proposed |
| [0016](0016-shared-base-index-and-worktree-overlays.md) | Shared base index with per-worktree overlays | Superseded by ADR-0028 |
| [0017](0017-surface-reduction.md) | Surface reduction — remove graph, web, shell, token-saving and review queue | Accepted (amended by ADR-0022) |
| [0018](0018-mcp-verb-tools.md) | MCP surface — verb tools with exact schemas | Accepted (amended by ADR-0020, ADR-0022, ADR-0030) |
| [0019](0019-library-crate-layering.md) | Library crate layering | Superseded by ADR-0023 |
| [0020](0020-verb-tool-contract-amendments.md) | Verb-tool contract — no output schemas, always-load scope, kept domain tools | Accepted (amended by ADR-0021, ADR-0022, ADR-0030) |
| [0021](0021-agent-comms-v2.md) | Agent comms v2 — stable identity, per-actor delivery, three deferred tools | Accepted (amended by ADR-0027, ADR-0030) |
| [0022](0022-surface-adr-amendments.md) | Surface ADR amendments — as-built facts and a measurable value gate | Accepted (amended by ADR-0027, ADR-0030, ADR-0031) |
| [0023](0023-library-crate-layering-revised.md) | Library crate layering, revised | Accepted (amended by ADR-0027, ADR-0032, ADR-0039) |
| [0024](0024-daemon-and-process-model.md) | Daemon and process model — rendezvous scoped by protocol generation | Accepted (amended by ADR-0028, ADR-0030) |
| [0025](0025-store-versioning-and-durability.md) | Store versioning and durability policy | Accepted (amended by ADR-0028, ADR-0030, ADR-0033, ADR-0037) |
| [0026](0026-release-and-branch-policy.md) | Release and branch policy — trunk-based, three breaking releases | Superseded by ADR-0027 |
| [0027](0027-single-breaking-release.md) | Release and branch policy — trunk-based, one breaking release | Accepted (amended by ADR-0035) |
| [0028](0028-content-keyed-group-index.md) | One content-keyed index per repository group | Accepted (amended by ADR-0036) |
| [0029](0029-document-retrieval-pipeline-and-store.md) | Document retrieval pipeline and retrieval store | Accepted (amended by ADR-0037) |
| [0030](0030-as-built-daemon-wire-and-store-rules.md) | As-built rules — daemon generations, verb-tool wire contract, store durability | Accepted (amended by ADR-0033, ADR-0038) |
| [0031](0031-value-gate-by-job.md) | Value gate by job — locating and reducing are judged differently | Accepted |
| [0032](0032-layering-as-built.md) | Library crate layering as built — module table, schema validation, open edges | Accepted (amended by ADR-0039) |
| [0033](0033-store-evidence-collection-and-recovery-rules.md) | Store rules after the sixth storage review — evidence, collection, recovery | Accepted |
| [0034](0034-lean-locating-rows.md) | Lean rows for the locating tools | Accepted |
| [0035](0035-unreleased-plugin-surface-marker.md) | A dated marker for plugin content ahead of its release | Accepted |
| [0036](0036-group-index-as-built.md) | Group index as built — no rollout switch, the group lock, read-only opens | Accepted |
| [0037](0037-retrieval-store-amendments.md) | Retrieval store amendments — liveness list, store version, group location, model identity | Accepted |
| [0038](0038-comms-cutover-as-built.md) | Comms v2 cutover as built — agent tool replay and the closed open items | Accepted |
| [0039](0039-engine-unification-and-two-crates.md) | Engine unification first, then two crates | Accepted (amended by ADR-0040) |
| [0040](0040-cross-harness-agent-launch.md) | Cross-harness agent launch — `basemind-agents` and `serve --lead` | Accepted |
