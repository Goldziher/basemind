---
status: accepted
date: 2026-11-18
amends: ["0029", "0025"]
---

# ADR-0037: Retrieval store amendments — liveness list, store version, group location, model identity

- **Deciders:** the repository owner
- **Related:** amends ADR-0029 (rules 1, 2, 3, 5, 7, 8, 10, 11) and ADR-0025 (the vector stores' version);
  ADR-0028

## Context

ADR-0029 was implemented on `feat/retrieval-pipeline`. Four of its clauses do not survive contact
with the code and the benchmark:

- **Rule 3** sends the live keys to the store as `doc_key IN (...)` while the list holds at most
  20,000 keys. The benchmark (`benchmarks/retrieval/PROBES.md`, `results/table.md`) measured a
  20,000-key list adding about 65 ms to the vector lane's p95 and 18 to 23 ms to the lexical lane's,
  at both tiers, against a budget of 10 ms (50,000 chunks) and 25 ms (250,000). A 2,000-key list
  adds under 5 ms. The `InList` expression form is 25 to 55% cheaper than SQL text with the BTree.
- **Rule 11** ties the store's version to the release minor, as ADR-0025 does for every derived
  store. For document rows that costs little, because blobs are re-extracted at each minor anyway.
  For the vector rows of memory notes it re-embeds every note at every minor, although nothing in
  their layout changed. The request on this branch was to decouple both stores from the minor,
  provided a model or layout change can never serve a stale vector.
- **Rule 2** puts the store under the repository group (ADR-0028), and rule 11 leaves deleting dead
  rows to the group sweep. The group directory and lock exist, and `Store::group_live_doc_keys`
  lists the document keys every view of the group names; the first implementation still kept one
  store per view, so two worktrees of a repository held, and embedded, the same rows twice.
- **Rules 5 and 7** name `kind`, `path` and `keywords` filters and a reranker that scores 50 hits,
  without saying what values they take or which setting holds the 50.
- **Rule 1** names HTML a document type, but the scan sent every file a tree-sitter grammar claims to
  the code map, so no HTML file reached the document store (15 of 86 eval tasks scored zero).
- **Rule 2** names the store by "model", and the code used the preset name. A preset name is
  xberg's: a release that points it at another model would mix two models' vectors in one store.
- **Rule 1** leaves JSON, YAML, TOML, CSV and logs to `code_grep`, but the scan sends them, and
  plain text, to the document tier, and `docs_search` says it searches config and data files.
  **Rule 10** keeps what xberg reports, and for these files xberg reports a rewrite: a plain-text
  paragraph or a whole log becomes one line, a JSON or YAML file a list of `key: value`
  paragraphs. No chunk of that text is found in the file again, so none cited a line.
- **Rule 11** says nothing of a document whose embedding failed. Indexed without vectors and marked
  attempted, it never got them, even once the embedder worked again.

## Decision

1. **Liveness list (amends rule 3).** The allowed keys go into the predicate, as a DataFusion
   `InList` on `doc_key`, while they number at most **2,000**
   (`docs_pipeline::LIVE_KEYS_PREFILTER`). Above that each lane fetches `max(3k, k + 64)`, keeps
   the live hits and doubles up to 4,096; a lane that reaches 4,096 with fewer than `k` live hits
   makes the response say `truncated_reason: "liveness_depth"`.
2. **Store version (amends rule 11 and ADR-0025's vector rows).** The LanceDB stores, the
   document store and the memory vectors, carry their own table-layout version, independent of
   the release minor (`lance::VECTOR_STORE_VER`, starting at 1000 so that every store a
   minor-numbered binary wrote reads as older). Older is wiped, newer refused, as before.
   Staleness is prevented by the keys, not by the wipe:
   - the store directory is named by the model and dimension, so another preset opens another
     directory and its rows are written there;
   - `doc_key` hashes the blob schema version with the content hash and the extraction digest, so
     rows of an older extractor have another key, are dead for every view, and are deleted;
   - memory vector rows are found by note key and checked against the note's `updated_at`
     (ADR-0025 (c)), and a missing row is rebuilt from the note.
   The version is bumped when a table's columns change.
3. **Group location (amends rules 2 and 11).** The store is
   `cache/groups/<group>/lance/<model-slug>-<dim>-<hash8>/` (`lance::docs::group_store_dir`), one
   per repository group and model; rows carry no path or workspace, so every worktree of the group
   reads and writes the same rows, and a document another worktree already wrote is not written
   again. A view searches only the keys its own document map names (rule 1 here). A scan deletes a
   row only when its key is in no view's document map (`Store::group_live_doc_keys`); when a view's
   map cannot be read the liveness is unknown and nothing is deleted. A store an earlier build kept
   under `views/<view>/retrieval/` is deleted by the first scan that opens the group store, which
   fills it from the cached blobs, without embedding again; the rows' shape is unchanged, so
   `VECTOR_STORE_VER` stays.
4. **Filter values (amends rule 5).** `kind` is one of `pdf`, `office`, `html`, `markdown`,
   `email`, `notebook`, `text`, `other`; `path` is a glob over the view's document paths;
   `keywords` is a comma-separated list, a document passing when it has any of them as a whole
   keyword, case-insensitive; `entity` is a whole entity category, case-insensitive.
5. **Rerank depth (amends rule 7).** `[documents.reranker] top_k` is the number of fused hits the
   cross-encoder reorders, default 50 and at most 50; the hits after them keep their fused order.
6. **Markup a grammar also claims (amends rule 1).** HTML, LaTeX, Org, Typst, Djot and RTF go to the
   document tier when it is enabled and the path reached the grammar by its own extension
   (`lang::is_document_markup`). A workspace with documents off, or a `[languages]` override mapping
   another extension to one of these grammars (`.erb` to `html`), keeps the file in the code map, so
   `code_grep` still reaches it. Staged and rev views index no documents: xberg extracts from a
   file, and the working tree's file is not the view's content.
7. **Model identity (amends rule 2).** Every stored vector, and the name of every vector store
   directory, carries `embeddings::model_key(preset)`: the preset name and a digest of what xberg
   loads for it (repository, model file, sibling files, pooling, dimension, backend, query prefix).
   A blob or store stamped with another key is re-embedded or opened anew. xberg's pinned model
   revision is not public, so a new upload under the same file name is caught only by the xberg
   upgrade that pins it.
8. **Vectors owed after a failed embed (amends rule 11).** A document whose embedder failed is
   indexed without vectors and marked as owing them (`FileMapDoc::embed_failed`, an entry neither
   embedded nor attempted). Each scan probes the embedder once, on the first document that owes
   vectors; while the probe fails the document stays settled, and once it passes the document is
   re-extracted and embedded without having changed.
9. **Compaction of short-lived writers.** Each table is compacted when it holds 32 small fragments
   and pruned every 32 versions, read from the table itself, besides the per-process commit
   counter: a `basemind memory save` process commits twice and exits, and never reached the
   counter's period.
10. **Headings inside a hit (amends rule 8).** A hit's `section` is the heading trail where its
    text starts; `headings` lists the headings that start inside it. A chunk of a short document
    spans several sections, and the trail alone names the one before the answer.
11. **Text and data files are chunked from the file (amends rules 1 and 10).** Plain text and
    logs, CSV and TSV, NDJSON, JSON, YAML and TOML are documents, chunked by basemind from the
    file's own text in one pass (`extract::doc::source`), not from xberg's rendering: prose and logs
    end a chunk at a blank line, else a line end, and overlap by whole lines; records (CSV, TSV,
    NDJSON) end at a line end without overlap, and a CSV or TSV chunk is embedded with the header
    line in front; trees (JSON, YAML, TOML) end before the least indented line of the window's back
    half that does not close a block. Each chunk's line range follows from its byte range, so every
    chunk cites its lines. xberg still extracts the file, for keywords, entities (placed again in
    the file's text), language and summary. These files are embedded like any other document,
    under `embed_include` / `embed_exclude` and the chunk cap. A Markdown chunk, which xberg does
    chunk, is found in its file with whitespace collapsed on both sides (xberg joins a soft-wrapped
    paragraph into one line), and by a run of plain text when a list marker, table padding or an
    entity was rewritten, so it cites its lines too.

## Consequences

- Upgrading wipes each vector store once (its old version is the minor, below 1000); after that a
  release minor no longer wipes the memory vectors.
- The worktrees of a repository share one copy of the document rows. Two worktrees whose
  `basemind.toml` embed policies differ for one document share its rows too: a view whose policy
  excludes the document strips its vectors only when no other view of the group names it, so a
  worktree that wants them keeps them. A document that two views name and both exclude keeps its
  vectors as long as both name it; its lexical rows are the same either way.
- A view whose allowed set exceeds 2,000 documents pays the widening on every query and may see
  `liveness_depth`.
- ADR-0029's `lang` column and its Bitmap index are not built: no filter reads them.
- HTML and the other markup stay searchable by `code_grep`, which reads the document tier's text
  files.
- The first scan of this version re-embeds every vector once: blobs stamped with a bare preset name
  do not match the model key.
- An offline machine pays one embedder probe per scan while documents owe vectors.
- The extraction digest moves to `doc/4`: every document is extracted again once, and re-embedded.
  A plain-text file no longer reaches xberg's Markdown splitter at any size.
- The reranker stays off by default. On the docs eval corpus `bge-reranker-base` raised nDCG@10 over all
  tasks (0.954 to 0.967) but lowered it on natural-language questions (0.954 to 0.946), dropped one gold
  out of the top ten, and cost about 25 times the latency.

## Alternatives considered

- **Keep 20,000 keys** — rejected: the benchmark measured the rule's own budget exceeded.
- **Keep the minor as the version** — rejected: every minor re-embeds every memory note for no
  layout change, and the model-named directory and versioned `doc_key` already keep stale rows
  from being served.
- **Copy the per-view stores into the group store** — rejected: the rows are rebuilt from the
  cached blobs without embedding, and the views of one group hold the same rows, so a copy would
  have to merge duplicates for no gain.
