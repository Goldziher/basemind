---
name: basemind-documents
description: >-
  Semantic + full-text search over documents and the web via basemind's RAG store — PDFs, Office,
  HTML, email, images (OCR), plus scraped/crawled web pages, with cross-encoder reranking, keyword
  and named-entity (NER) filters, and per-document summaries. Reach for it whenever the user asks to
  "search the docs / PDFs", "find where a topic is discussed", "pull this URL into context", or
  "what does the documentation say about X".
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:0502f17289c5e74c29ad8b882f90c407fa162f245e45297cbd7a1c414beddbad
Source-Hash: blake3:790db1582bcc08e9525d458dbfdd394dcb874071a62cc78df0d39a2dcd669ee7
Schema-Version: v1
-->

# basemind-documents — document RAG and web ingestion

basemind extracts 90+ file formats (PDF, Office, HTML, email, images via OCR) into a LanceDB vector
store and answers meaning-based queries with cross-encoder reranking. Web pages scraped or crawled
into the same store are searchable the same way. This is the surface for "find the passage about X",
not "grep for the string X".

**basemind first, open-the-file fallback.** Prefer `memory` mode `documents` over opening PDFs/Office/HTML
by hand, and the web tools over ad-hoc fetching. For source code use `basemind-code-search` instead —
this skill is for prose and documents.

## Requirements

- `memory` mode `documents` needs a build with `--features documents` (or `full`); the other memory
  modes need `--features memory`.
  Without them the tools dispatch but return an MCP error.
- Web ingestion (`web` modes `scrape` / `crawl` / `map`) needs `--features crawl`. When that feature
  is **off** these tools are not registered at all — they simply won't appear in the tool list.
- Documents must be scanned first: `basemind scan` with the documents feature extracts and embeds
  them into the machine-global cache (Linux `~/.local/share/basemind/`, macOS
  `~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`). See the `basemind-scan` skill.

## Tool routing

| Question | MCP tool | CLI |
|---|---|---|
| "Semantic search over PDFs/Office/HTML docs?" | `memory { mode: "documents", query: "…" }` | `basemind memory documents "query"` |
| "Narrow to docs mentioning an entity?" | `memory { mode: "documents", query: "…", entity_category: "…" }` | *(MCP only)* |
| "Narrow to docs with a keyword?" | `memory { mode: "documents", query: "…", keywords_contains: "…" }` | *(MCP only)* |
| "Filter by file type?" | `memory { mode: "documents", query: "…", mime_type: "application/pdf" }` | `basemind memory documents "…" --mime-type application/pdf` |
| "Pull a single URL into RAG?" | `web { mode: "scrape", url: "…" }` (robots-aware) | `basemind web scrape <url>` |
| "Ingest a docs site section?" | `web { mode: "crawl", url: "…" }` | `basemind web crawl <seed-url>` |
| "What URLs exist on this site?" | `web { mode: "map", url: "…" }` | `basemind web map <url>` |
| "Recall something the agent stored earlier?" | `memory` mode `get`, `list`, or `search` | `basemind memory get "key"` / `list` / `search "q"` |
| "Remember this for future sessions?" | `memory { mode: "put", key, value }` | `basemind memory put "key" "value"` |

## What a hit carries

`memory` mode `documents` returns chunk-level hits with `path`, `chunk_idx`, the matched `text`, byte span,
vector `distance`, and — when enabled at scan time — a cross-encoder `rerank_score` in `[0,1]`, the
parent document's `keywords` and named `entities` (NER), and a document-level `summary`. Use
`entity_category` / `keywords_contains` to constrain to documents whose parent carries a matching
entity or keyword (AND-combined when both are set).

## Examples

```text
memory { mode: "documents", query: "how is the index schema versioned", limit: 5 }
→ docs/architecture.pdf#chunk3  rerank 0.91  "INDEX_SCHEMA_VER reads from RELEASE_MINOR…"
  README.md#chunk12             rerank 0.74  "…wipe-on-mismatch rebuilds from source…"

web { mode: "crawl", url: "https://docs.example.com/guide" }
→ ingested 24 pages under scope "web:docs.example.com"

memory { mode: "documents", query: "rate limiting", mime_type: "text/html" }
→ web:docs.example.com/limits#chunk1  rerank 0.88  "requests are capped at …"
```

## Notes

- Crawled/scraped pages land in the `documents` table tagged with a `scope` of `web:<host>`
  (override in `web` mode `scrape`); `memory` mode `documents` searches every ingested document.
- `robots.txt` is honoured by default; only `[crawl].respect_robots_txt = false` in
  the repo-root `basemind.toml` (config-file-only) disables it. The crawler SSRF-blocks private/loopback
  hosts unless `[crawl].allow_private_network = true`.
- Memory is scoped by the normalised git `origin` URL, so clones of the same repo share stored
  entries and unrelated repos do not.
- Lists are capped (`limit`, default 100, max 1000); use `next_cursor` → `cursor` to page.

For code structure see `basemind-code-search`; for git history see `basemind-git-history`; for agent
coordination see `basemind-comms`.
