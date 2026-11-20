# Documents

The code map holds code only. Prose, data and config files (Markdown, reStructuredText, CSV, JSON,
YAML, TOML, XML, INI and the like) and binary documents (PDF, Office, HTML, email, notebooks) form
the document tier: a scan extracts and chunks them, and `docs_search` ranks the passages by meaning
and by words, then cites each hit (path, pages, section, lines).

| Question | MCP call | CLI |
|---|---|---|
| Where is a topic discussed? | `docs_search {"query":"how releases are versioned"}` | `basemind memory documents "how releases are versioned"` |
| Only PDFs | `docs_search {"query":"…","kind":"pdf"}` | `basemind memory documents "…" --kind pdf` |
| Only under a directory | `docs_search {"query":"…","path":"docs/"}` | `basemind memory documents "…" --path docs/` |
| Only paths matching a glob | `docs_search {"query":"…","path":"docs/**/*.md"}` | `basemind memory documents "…" --path "docs/**/*.md"` |
| One exact MIME type | `docs_search {"query":"…","mime_type":"application/pdf"}` | `basemind memory documents "…" --mime-type application/pdf` |
| Documents with a keyword | `docs_search {"query":"…","keywords":"rate limit"}` | `basemind memory documents "…" --keywords "rate limit"` |
| Documents naming a person | `docs_search {"query":"…","entity":"PERSON"}` | `basemind memory documents "…" --entity PERSON` |

## Contracts

- `docs_search` needs a build with the `documents` feature (the release binary has it); without it
  the call is an error naming the feature. Documents must be scanned first: `basemind scan`.
- `kind` is one of `pdf`, `office`, `html`, `markdown`, `email`, `notebook`, `text`, `other`.
  `path` is a repository-relative prefix, or a glob over the whole path when it holds `*`, as in
  `code_grep` and `find_files`. Filters apply before ranking.
- `keywords` are whole keywords, comma-separated, any one matching. `entity` keeps chunks naming an
  entity of that category and needs a build with named-entity recognition; without it the filter is
  an error, not an empty result.
- Plain text, logs, CSV, JSON, YAML and TOML are chunked from the file itself, so every hit cites
  its lines. `code_grep` still searches them as text and `find_files` still lists them.
- Archives (`.zip`, `.tar`, …) are never extracted. Which files become documents, the embedding
  model and the size and time limits are set in the `[documents]` table of `basemind.toml`.
