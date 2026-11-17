---
status: proposed
date: 2026-11-17
amends: ["0029", "0025"]
---

# ADR-0037: Retrieval store amendments — liveness list, store version, interim location, model identity

- **Deciders:** the repository owner
- **Related:** amends ADR-0029 (rules 1, 2, 3, 5, 7, 11) and ADR-0025 (the vector stores' version);
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
- **Rule 2** puts the store under the repository group (ADR-0028). The group directory, lock and
  live-key map are being built on another branch and do not exist here.
- **Rules 5 and 7** name `kind`, `path` and `keywords` filters and a reranker that scores 50 hits,
  without saying what values they take or which setting holds the 50.
- **Rule 1** names HTML a document type, but the scan sent every file a tree-sitter grammar claims to
  the code map, so no HTML file reached the document store (15 of 86 eval tasks scored zero).
- **Rule 2** names the store by "model", and the code used the preset name. A preset name is
  xberg's: a release that points it at another model would mix two models' vectors in one store.
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
3. **Interim location (amends rule 2 until ADR-0028 lands).** The store is
   `views/<view>/retrieval/<model-slug>-<dim>-<hash8>/` in the workspace cache, chosen in one
   function, `lance::docs::store_dir(parent, model, dim)`; rows carry no path or workspace. Each
   view has its own store because each view's scan deletes the rows its document map does not
   name; two views sharing one would delete each other's rows. When the group directory exists,
   `store_dir` is called with it and that deletion moves to the group sweep, which passes every
   view's live keys.
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

## Consequences

- Upgrading wipes each vector store once (its old version is the minor, below 1000); after that a
  release minor no longer wipes the memory vectors.
- Each view of each worktree holds its own copy of the document rows until ADR-0028's group
  directory exists.
- A view whose allowed set exceeds 2,000 documents pays the widening on every query and may see
  `liveness_depth`.
- ADR-0029's `lang` column and its Bitmap index are not built: no filter reads them.
- HTML and the other markup leave `code_grep` in a workspace with documents on, as Markdown did.
- The first scan of this version re-embeds every vector once: blobs stamped with a bare preset name
  do not match the model key.
- An offline machine pays one embedder probe per scan while documents owe vectors.
- The reranker stays off by default. On the docs eval corpus `bge-reranker-base` raised nDCG@10 over all
  tasks (0.954 to 0.967) but lowered it on natural-language questions (0.954 to 0.946), dropped one gold
  out of the top ten, and cost about 25 times the latency.

## Alternatives considered

- **Keep 20,000 keys** — rejected: the benchmark measured the rule's own budget exceeded.
- **Keep the minor as the version** — rejected: every minor re-embeds every memory note for no
  layout change, and the model-named directory and versioned `doc_key` already keep stale rows
  from being served.
- **Build the group directory here** — rejected: it is ADR-0028's work, in progress elsewhere.
