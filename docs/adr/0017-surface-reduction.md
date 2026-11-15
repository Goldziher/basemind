---
status: accepted
date: 2026-11-14
supersedes: ["0001", "0002", "0003", "0004", "0005", "0007", "0008", "0009"]
amends: ["0011"]
---

# ADR-0017: Surface reduction — remove graph, web, shell, token-saving and review queue

- **Status:** Accepted; amended by ADR-0022 (2026-11-15)
- **Date:** 2026-11-14
- **Deciders:** basemind maintainers
- **Related:** supersedes ADR-0001, ADR-0002, ADR-0003, ADR-0004, ADR-0005, ADR-0007, ADR-0008,
  ADR-0009; amends ADR-0011 (domain count; ADR-0011 is itself superseded by ADR-0018)

## Context

basemind's surface grew to nine MCP domains and a matching CLI: a typed code graph with traversal,
communities and renderers, a web crawler, managed terminal sessions, token-saving output tools, and
a co-change proposal queue for memory. Measurements show most of it is unused or does not deliver:

- **Usage.** Telemetry over three months (41k calls) records zero `graph` calls and zero `web` calls.
  Five `code` modes (`grep`, `outline`, `expand`, `symbols`, `find`) make up 94.6% of `code` calls.
- **Quality.** A live test of `graph map` on this repository returned an uninformative module graph:
  every edge inferred at 0.5 confidence, and one strongly-connected component holding every module.
  The module edges are aggregated from name-only call matches.
- **Cost.** The removal is about 27k lines of code and tests and 21 lockfile packages. The `shells`
  feature depends on the `rmux-*` crates by git revision, which is one of two blockers to
  publishing on crates.io.
- **Advertising.** The nine-domain tool list costs about 13.6k tokens in `tools/list`.

## Decision

basemind's product is code mapping, code search, fuzzy search, document RAG, git history, plain
shared memory and agent messaging on local files. Everything else is deleted entirely: MCP, CLI,
code, features, tests and docs.

| Removed | Scope |
|---|---|
| `graph` domain | all nine modes, the graph/community/traversal/rendering code, the doc-links chain and the `/ui` HTTP route |
| `web` domain | `scrape`, `crawl`, `map`, `src/web/`, the `crawl` feature and its `crawlberg` dependency |
| `shell` domain | `src/shells/`, the `shells` feature and the `rmux-*` git dependencies |
| `admin` token-saving modes | `compress`, `delta`, `checkpoint`, `waste`, `src/textcompress/`, the `compress-output` and `read-cache` hooks |
| `memory` review queue | `mine`, `proposals`, `accept`, `reject`, `audit`, and the governance code and keyspace readers |

Plain memory (`put`, `get`, `list`, `search`, `delete`), `code` and `git` are untouched. `code`
`callers`, `references`, `implementations`, `dependents` and `definition` read the index directly
and do not depend on the graph code. `code expand` shares files with the compress modes and moves
before those files are deleted.

## Consequences

- ADR-0001, 0002, 0003, 0004, 0005, 0007, 0008 and 0009 are superseded by this ADR. ADR-0011 fixes
  the surface at nine domains; this ADR amends that count, and ADR-0018 then supersedes ADR-0011.
- Compatibility measures that stay:
  - `[crawl]` and `[shells]` config tables are tolerated and ignored, because `ConfigV1` rejects
    unknown keys and `basemind init` has written `[crawl]` into existing `basemind.toml` files.
  - The `Shells` daemon kind stays readable for one release, so an old daemon is still found and
    can be stopped instead of being treated as a corrupt record and orphaned.
  - The comms protocol version goes from 4 to 5, because the governance request type is removed
    and version skew must fail at the handshake.
  - The `rationale` field stays in the blob format until the next minor schema bump; dropping it
    changes the persisted schema.
- Persisted data from removed features is left in place and never refreshed: the `doc_links`
  table in LanceDB, the `proposals` and `memory_archive` keyspaces in `memory.fjall`, and
  documents indexed under `web:` scopes.
- The config schema is regenerated and the lockfile loses its crawl and `rmux-*` closures.
- Removal lands as serial commits (token-saving, web, shell, governance, graph), each updating the
  smoke, parity and harden suites in the same commit.

## Alternatives considered

- **Hide the tools from MCP but keep them in the CLI** — rejected: it keeps the maintenance cost,
  the dependency closure and the publishing blocker for features with no measured use.
- **Keep `graph calls` only** — rejected: `code` `callers` and `references` already answer it from
  the index, without a second traversal engine.
