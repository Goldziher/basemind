---
description: Semantic search over documents via basemind's RAG store — PDFs, Office, HTML, email, Jupyter notebooks, and images (OCR), with cross-encoder reranking, keyword and named-entity (NER) filters, and per-document summaries. Reach for it whenever the user asks to "search the docs / PDFs", "find where a topic is discussed", or "what does the documentation say about X".
name: basemind-documents
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:3c3e413ac2eecc5d3c8a0e25dc445e61d860fd29eed19e1d38564be2a853a5cd
Source-Hash: blake3:e09c651f27bce33463989a697aa75fe5c51be77ecaba769384a73c6adde83aa0
Schema-Version: v1
-->

# basemind-documents — document RAG

basemind extracts 90+ file formats (PDF, Office, HTML, email, images via OCR) into a LanceDB vector
store and answers meaning-based queries with cross-encoder reranking. For exact matches in text files such as Jupyter
notebooks, use a literal or regex file search. Semantic results cannot establish that a string is absent.

**basemind first, open-the-file fallback.** Prefer `memory` mode `documents` over opening PDFs/Office/HTML
by hand. For source code use `basemind-code-search` instead —
this skill is for prose and documents.

## Requirements

- `memory` mode `documents` needs a build with `--features documents` (or `full`); the other memory
  modes need `--features memory`.
  Without them the tools dispatch but return an MCP error.
- Documents must be scanned first: `basemind scan` with the documents feature extracts and embeds
  them into the machine-global cache (Linux `~/.local/share/basemind/`, macOS
  `~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`). See the `basemind-scan` skill.

## Tool routing

| Question | MCP tool | CLI |
|---|---|---|
| "Semantic search over PDFs/Office/HTML docs?" | `memory { mode: "documents", query: "…" }` | `basemind memory documents "query"` |
| "Search Jupyter notebooks by meaning?" | `memory { mode: "documents", query: "…", mime_type: "application/x-ipynb+json" }` | `basemind memory documents "…" --mime-type application/x-ipynb+json` |
| "Narrow to docs mentioning an entity?" | `memory { mode: "documents", query: "…", entity_category: "…" }` | *(MCP only)* |
| "Narrow to docs with a keyword?" | `memory { mode: "documents", query: "…", keywords_contains: "…" }` | *(MCP only)* |
| "Filter by file type?" | `memory { mode: "documents", query: "…", mime_type: "application/pdf" }` | `basemind memory documents "…" --mime-type application/pdf` |
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

memory { mode: "documents", query: "rate limiting", mime_type: "text/html" }
→ docs/site/limits.html#chunk1  rerank 0.88  "requests are capped at …"
```

## Notes

- `memory` mode `documents` searches this repo's scope by default. Web ingestion was removed: pages
  an older version scraped or crawled stay in the store under `web:<host>` and are no longer
  refreshed; pass `scope: "web:<host>"` to search them.
- Memory is scoped by the normalised git `origin` URL, so clones of the same repo share stored
  entries and unrelated repos do not.
- Lists are capped (`limit`, default 100, max 1000); use `next_cursor` → `cursor` to page.

For code structure see `basemind-code-search`; for git history see `basemind-git-history`; for agent
coordination see `basemind-comms`.
