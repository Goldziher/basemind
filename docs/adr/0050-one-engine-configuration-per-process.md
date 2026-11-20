---
status: accepted
date: 2026-11-20
amends: ["0039"]
---

# ADR-0050: One engine configuration per process, and no environment below the root layer

- **Deciders:** the repository owner
- **Related:** amends ADR-0039; ADR-0023 rule 2 (the data root) and rule 3 (the retrieval runtime),
  ADR-0024 (daemon and process model)

## Context

ADR-0039 makes the engine a synchronous library that an embedder takes without the CLI. Until
engine step S9 the library read about 30 `BASEMIND_*` variables wherever it used them: the data
home, the operator's trust grants, parse, grammar, cache, disk, git, index and grep knobs, and
the daemon ceilings. An embedder could configure it only by setting process environment, and one
variable's meaning was spread over the module that read it.

Some state below the root layer is process-wide by nature and cannot belong to one value:

- the blob store, the group indexes and the daemon registry under the data home, and the
  Fjall handles that lock them;
- the tree-sitter parser pool and the language pack's grammar registry;
- the resource ceilings: the cached memory reading (`sysres::CACHE_*`), the footprint admissions
  and the document semaphore (`backpressure`), and the ledger of abandoned extraction threads
  (`doc_guard::AbandonedThreads::global`).

## Decision

1. **One installed configuration per process.** `EngineConfig` carries every operator setting.
   `engine::Engine::new` installs it, and core, retrieval and engine code read the installed value
   (`config::engine::with` / `current`). A process that installs none gets
   `EngineConfig::default()`: the platform data home and the documented defaults.
2. **A second engine shares the first.** `Engine::new` with a configuration equal to the installed
   one returns another handle on it. Any other configuration is refused with
   `EngineError::Unavailable`, so no engine's `config()` disagrees with what the library uses. The
   data home is compared as a directory: paths equal component by component, or canonicalised to
   one path (a symlinked spelling is the same home). `config::engine::update` is a test seam that
   bypasses the rule and is compiled only for tests and the `test-support` feature.
3. **No environment reads below the root layer.** `cli::env_config` is the one place that maps
   the documented variables onto `EngineConfig`, with each variable's former parsing. A relative
   `BASEMIND_DATA_HOME` or `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR` is resolved against the current
   directory there, once. `tests/layering.rs` (`no_env_reads_below_root`) fails on an `env::var`
   family read under any alias of `std::env`, and on a `directories::{ProjectDirs, BaseDirs,
   UserDirs}` constructor, which read `HOME` and `XDG_*` implicitly. The allowed exceptions are
   listed with counts: the `Env` secret a `basemind.toml` names by its variable, and the two
   `ProjectDirs` lookups that define the platform defaults (the data home, the user-level agents
   policy).
4. **Children get the installed data home.** The comms daemon spawn sets `BASEMIND_DATA_HOME` from
   the installed `DataRoot`, and a launched agent gets it through `LaunchConfig::data_home`, which
   also guards the launch root in place of the launcher's variable. An embedder that configured
   its data home in code does not split its state from the daemon it starts.
5. **Grammars live under the data home** (`<data home>/grammars`) unless the operator sets
   `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR`, which keeps its meaning. The language pack is
   configured once per process, before its first use.
6. **The process-wide ceilings stay statics.** The memory reading, the footprint admissions, the
   document semaphore and the abandoned-thread ledger measure or bound the process, not a
   configuration: a second engine would share the same memory and the same OS threads, and an
   abandoned extraction thread outlives any handle that could own its count. With one
   configuration per process a field on the engine would be the same value threaded through
   every rayon worker.
7. **Retrieval builds its runtimes in one module** (`lance::runtime`, ADR-0023 rule 3): document
   extraction's multi-thread runtime, and one current-thread runtime per vector store that the
   store shuts down when dropped. They never share a blocking pool: an extraction past its
   deadline is abandoned while its blocking work keeps its thread, so a shared pool would let hung
   parses hold up every search, and a dropped store's spawned lance tasks would keep its file
   handles open on a runtime that outlives it.

## Consequences

- An embedder configures basemind in code and may run several engine handles in one process only
  if they agree; a second data home needs a second process.
- Every variable's meaning, default and parsing is in one file, and its tests are table rows.
- The hot-path reads (`lang::parse_timeout` twice per file on every scan worker) go through a
  per-thread snapshot refreshed by an install generation, not the shared lock.
- Tests isolate through `store::init_isolated_cache`, which installs a throwaway data home and
  keeps the grammar cache shared. An in-process entry point (`mcp::serve_in_memory`) refuses to
  run when `BASEMIND_DATA_HOME` names another home than the installed one.
- Existing grammar caches in the platform cache dir are not reused unless the operator points
  `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR` at them; the grammars are downloaded once into the data
  home. Processes that cargo starts (tests, benches, `cargo run`) get one shared grammar cache
  from `.cargo/config.toml`, since each test spawns basemind with a throwaway data home.

## Alternatives considered

- **Last install wins.** Rejected: an earlier engine's `config()` would report settings the library
  no longer uses, trust grants included.
- **Per-engine configuration threaded to every reader.** Rejected: the stores, the parser pool and
  the ceilings it would configure are process-wide, so two engines could not hold different
  values for them anyway, and every core signature would grow a parameter.
- **Keep reading the environment and document the variables as the API.** Rejected: an embedder
  could not configure two settings without mutating process environment, which is unsound once
  threads run.
- **One shared retrieval runtime.** Rejected: see decision 7.
