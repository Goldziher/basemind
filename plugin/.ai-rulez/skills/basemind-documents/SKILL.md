---
type: Playbook
title: Basemind Documents
description: Semantic search over documents via basemind's
  RAG store — PDFs, Office, HTML, email, Jupyter notebooks, and images (OCR),
  with cross-encoder reranking, keyword and named-entity (NER) filters, and per-document
  summaries. Reach for it whenever the user asks to "search the docs / PDFs", "find
  where a topic is discussed", or "what does the documentation say about X".
x-ai-rulez:
  kind: skill
  id: basemind-documents
  metadata:
    name: basemind-documents
    license: MIT
    metadata:
      author: Goldziher
      repository: https://github.com/Goldziher/basemind
---

# basemind-documents — document search and memory

basemind extracts PDFs, Office files, spreadsheets, email, HTML and notebooks (and, in a build with
OCR, raster images and scanned PDFs) into a LanceDB vector
store and answers meaning-based queries with cross-encoder reranking. For exact matches in text
files such as Jupyter notebooks, use a literal or regex file search. Semantic results cannot
establish that a string is absent.

Prefer `docs_search` over opening PDFs/Office/HTML by hand. The tools are always loaded: call them
directly, no tool search needed. For source code use `basemind-code-search` instead; this skill is
for prose and documents.

## Requirements

- `docs_search` needs a build with `--features documents` (or `full`); `memory_save` and
  `memory_search` need `--features memory`. Without the feature the tool answers with an error
  result naming it. The release binary includes both.
- OCR and entity filtering depend on the binary's build: OCR is the `documents-ocr` Cargo feature
  and named-entity recognition is `documents-ner`. The published binary has both. On a build
  without OCR, images are not indexed and a scanned PDF (no text layer) is indexed with no text; on
  a build without NER, an `entity` filter returns an error, not an empty result.
- Archives (`.zip`, `.tar`, ...) are never extracted, so their contents are not searchable.
- Documents must be scanned first: `basemind scan` with the documents feature extracts and embeds
  them into the machine-global cache (Linux `~/.local/share/basemind/`, macOS
  `~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`). See the
  `basemind-scan` skill.

## Tool routing

| Question | MCP call | CLI |
|---|---|---|
| "Search the docs by meaning?" | `docs_search {"query":"how releases are versioned"}` | `basemind memory documents "how releases are versioned"` |
| "Search Jupyter notebooks by meaning?" | `docs_search {"query":"…","mime_type":"application/x-ipynb+json"}` | `basemind memory documents "…" --mime-type application/x-ipynb+json` |
| "Filter by file type?" | `docs_search {"query":"…","mime_type":"application/pdf"}` | `basemind memory documents "…" --mime-type application/pdf` |
| "Narrow to docs naming an entity category?" (needs a build with `documents-ner`) | `docs_search {"query":"…","entity":"PERSON"}` | `basemind memory documents "…" --entity PERSON` |
| "Narrow to docs with a keyword?" | `docs_search {"query":"…","keywords":"rate limit"}` | `basemind memory documents "…" --keywords "rate limit"` |
| "Remember this for future sessions?" | `memory_save {"key":"decision/auth-tokens","value":"Tokens rotate every 24h.","tags":["auth"]}` | `basemind memory put decision/auth-tokens "Tokens rotate every 24h." --tag auth` |
| "Recall a note by meaning?" | `memory_search {"query":"token rotation"}` | `basemind memory search "token rotation"` |
| "Read one note by key?" | `memory_search {"key":"decision/auth-tokens"}` | `basemind memory get decision/auth-tokens` |
| "List notes under a key prefix?" | `memory_search {"prefix":"decision/"}` | `basemind memory list --prefix decision/` |
| "Delete a note?" | CLI only | `basemind memory delete decision/auth-tokens` |

## What a hit carries

`docs_search` returns chunk-level hits with `path`, `chunk_idx`, the matched `text`, byte span,
vector `distance`, and, when enabled at scan time, a cross-encoder `rerank_score` in `[0,1]`, the
parent document's `keywords` and named `entities` (NER, only from a build with `documents-ner`),
and a document-level `summary`. `entity` and `keywords` constrain to documents whose parent
carries a matching entity category or keyword (AND-combined when both are set).

## Examples

```text
docs_search {"query":"how is the index schema versioned","limit":5}
→ docs/architecture.pdf#chunk3  rerank 0.91  "INDEX_SCHEMA_VER reads from RELEASE_MINOR…"
  README.md#chunk12             rerank 0.74  "…wipe-on-mismatch rebuilds from source…"

docs_search {"query":"rate limiting","mime_type":"text/html"}
→ docs/site/limits.html#chunk1  rerank 0.88  "requests are capped at …"
```

## Notes

- `docs_search` searches this repo's scope by default. Web ingestion was removed: pages an older
  version scraped or crawled stay in the store under `web:<host>` and are no longer refreshed; pass
  `"scope":"web:<host>"` to search them.
- `memory_search` takes one selector: `query` (by meaning, ranked, no `cursor`), `key` (one note)
  or `prefix` (list by key prefix); with none it lists everything. `tag` filters a search or a
  list. Saving an existing `key` replaces the note.
- Memory is scoped by the normalised git `origin` URL, so clones of the same repo share stored
  entries and unrelated repos do not. `"visibility":"individual"` keeps a note to this agent.
- `docs_search` returns 10 hits by default; lists take `limit` and page with `next_cursor` ->
  `cursor`. An argument a tool does not take is rejected with a did-you-mean and the accepted list.

For code structure see `basemind-code-search`; for git history see `basemind-git-history`; for agent
coordination see `basemind-comms`.
