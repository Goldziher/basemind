---
priority: high
---

# Scanner Pipeline

`basemind scan` is the engine. Two entry points: full scan (`scanner::scan`) and incremental (`scanner::scan_paths`). Both share the same per-file pipeline.

```text
Walker (gitignore-aware)
  → filter by user glob + size cap
  → rayon par_iter
    → process_file(rel, contents):
        lang::detect()                — TSLP extension → LangId (or skip)
        L1 outline   (always)         — extract::l1
        L2 calls     (eager if cfg)   — extract::l2
        Store::write_l1               — content-addressed msgpack blob
        Store::write_l2 (if eager)
        IndexWriter::upsert_file(...) — Fjall secondary index
        per-file commit               — atomic batch
  → collect FileResult { rel, l1_hash, l2_hash?, … }
  → apply_outcomes:
        write Index meta
        prune deleted files via IndexWriter::remove_file
```

## Key invariants

- **Per-file commit**: every `process_file` commits its Fjall batch before returning. Fjall handles cross-thread locking; the scanner does not.
- **Atomic upsert**: `IndexWriter::upsert_file` is read-before-write — it reads existing primary entries first to derive secondary-index keys for deletion, then stages all deletes + inserts in one batch. No torn state on re-scan.
- **Eager L2 cost**: scanning TypeScript at ~81 k files takes ~22 s with eager L2 on (the default) and ~17 s with eager L2 off. Document the trade-off when enabling; offer the `eager_l2 = false` escape hatch.
- **`scan_paths` removal mirror**: when a file disappears between scans, `scan_paths` calls `IndexWriter::remove_file` so secondary indexes don't leak stale entries.
- **No `tokio::spawn`** on the scanner path — rayon `par_iter` is the parallelization unit.

### Where to extend

- Language detection is fully dynamic via `tree_sitter_language_pack::detect_language` → `LangId = &'static str` (the pack name). All ~300 TSLP grammars dispatch on first sight; the scanner does no per-extension special-casing.
- Richer outlines for a new language → drop a `src/queries/<pack-name>.scm` override with `;; section: symbols / imports / calls / docs` sections. See the `language-support` skill.
- Languages without an override fall through to the TSLP `tags.scm` fallback in `lang::try_get_query`: `tree_sitter_language_pack::get_tags_query(lang)` → `lang::adapt_tslp_tags` rewrites `@definition.*` / `@reference.call` captures into basemind's `@symbol.*` / `@call.*` shape, the result is cached per-lang and per-`(lang, kind)`. Covers ~100 grammars in the published TSLP bundle today (kotlin, csharp, swift, cpp, scala, solidity, …). Languages with neither an override nor an upstream `tags.scm` (JSON, YAML, TOML, …) still parse and land in `list_files`; symbol/call extraction yields empty vectors.
- New extraction tier (e.g. `l4` semantic types) → mirror the `l1`/`l2` shape: `extract/l4.rs`, blob suffix `.l4.msgpack`, optional eager toggle in `ScanConfig`.
- New index partition → see the `index-keyspace-evolution` skill.
