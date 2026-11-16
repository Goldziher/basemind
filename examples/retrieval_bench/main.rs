//! Backend benchmark for the document-retrieval redesign (ADR-0029 rule 12): LanceDB as it is (arm A),
//! LanceDB with scalar and full-text indexes on (arm B), and a Fjall + flat-file store that links
//! nothing new (arm C).
//!
//! ```text
//! retrieval_bench gen <n> <dim> <seed> <corpus-dir>
//! retrieval_bench truth <corpus-dir> <base|appended>
//! retrieval_bench build <A|B|C> <ingest|index|append|optimize> <corpus-dir> <store-dir>
//! retrieval_bench query <A|B|C> <corpus-dir> <store-dir> --selectivity <1|10|100> --live <100|90|50>
//!                       --state <base|appended|optimized> [--key-list <count>]
//! retrieval_bench probe <nulls|fts-no-index> <scratch-dir>
//! retrieval_bench probe inlist <corpus-dir> <store-dir> --keys <count>
//! ```
//!
//! One process per measurement, so peak RSS and the cold open are clean; `benchmarks/retrieval/run.sh`
//! drives the matrix. Every measurement is one NDJSON line on stdout. Every directory is an explicit
//! argument: the harness never resolves basemind's data home.
#![cfg(feature = "documents")]

mod backend;
mod corpus;
mod fjall_flat;
mod lance_flat;
mod lance_indexed;
mod probe;
mod truth;

use std::path::{Path, PathBuf};
use std::time::Instant;

use ahash::AHashSet;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use backend::{Backend, Filter, Hit, VectorMode};
use corpus::{CHUNKS_PER_DOC, Corpus, Params, doc_key, open_ipc};
use fjall_flat::FjallFlat;
use lance_flat::LanceFlat;
use lance_indexed::{AnnParams, LanceIndexed};
use truth::{MAX_PREDICATE_KEYS, Truth, percentile, quality, variant};

/// Untimed queries before the timed passes.
const WARMUP_QUERIES: usize = 20;
/// Timed passes over the query set. The pass with the lowest p95 is reported, so a burst of
/// unrelated load on the machine has to cover every pass to distort a measurement.
const TIMED_PASSES: usize = 3;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write `params.json`, `corpus.arrow`, `append.arrow` (1% more rows) and `queries.json`.
    Gen {
        n: u64,
        dim: usize,
        seed: u64,
        corpus_dir: PathBuf,
    },
    /// Write `truth.<state>.json` by brute force.
    Truth { corpus_dir: PathBuf, state: String },
    Build {
        arm: Arm,
        stage: Stage,
        corpus_dir: PathBuf,
        store_dir: PathBuf,
        #[command(flatten)]
        knobs: Knobs,
    },
    Query {
        arm: Arm,
        corpus_dir: PathBuf,
        store_dir: PathBuf,
        #[arg(long, default_value_t = 100)]
        selectivity: u32,
        /// Percent of documents in the worktree's live-key map. Below 100 the live keys are a
        /// `doc_key IN (…)` predicate, or a projection after ranking when the list is too long.
        #[arg(long, default_value_t = 100)]
        live: u64,
        /// Instead of a liveness share, pre-filter on a list of exactly this many keys (padded with
        /// absent keys past the corpus) and report latency only: the cost of the liveness list.
        #[arg(long)]
        key_list: Option<usize>,
        #[arg(long, default_value = "base")]
        state: String,
        /// Candidates fetched per lane (design section 2.3).
        #[arg(long, default_value_t = 200)]
        k: usize,
        #[command(flatten)]
        knobs: Knobs,
    },
    Probe {
        #[command(subcommand)]
        probe: ProbeCommand,
    },
}

#[derive(Subcommand)]
enum ProbeCommand {
    Nulls {
        scratch_dir: PathBuf,
    },
    FtsNoIndex {
        scratch_dir: PathBuf,
    },
    Inlist {
        corpus_dir: PathBuf,
        store_dir: PathBuf,
        #[arg(long)]
        keys: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Arm {
    #[value(name = "A")]
    A,
    #[value(name = "B")]
    B,
    #[value(name = "C")]
    C,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Stage {
    Ingest,
    Index,
    Append,
    Optimize,
}

#[derive(Args, Clone)]
struct Knobs {
    /// Arm B: pre-filtered row counts at or below this are searched exactly.
    #[arg(long, default_value_t = 50_000)]
    exact_max_rows: usize,
    #[arg(long)]
    nprobes: Option<usize>,
    #[arg(long)]
    refine_factor: Option<u32>,
    #[arg(long)]
    ef: Option<usize>,
    /// Arm B: IVF partitions at index build.
    #[arg(long)]
    partitions: Option<u32>,
    /// Arm B: also build the IVF_HNSW_SQ vector index in the `index` stage.
    #[arg(long)]
    ann: bool,
    /// Arm C: SQ8 candidates rescored against f32, per requested hit.
    #[arg(long, default_value_t = 4)]
    rescore_factor: usize,
    /// Arms A and B: drive Lance on a current-thread runtime, as `LanceStore` does.
    #[arg(long)]
    current_thread: bool,
    /// Free-form tag copied into every output line, to tell tuning runs apart.
    #[arg(long, default_value = "")]
    label: String,
}

impl Knobs {
    fn json(&self) -> Value {
        json!({
            "exact_max_rows": self.exact_max_rows, "nprobes": self.nprobes,
            "refine_factor": self.refine_factor, "ef": self.ef, "partitions": self.partitions, "ann": self.ann,
            "rescore_factor": self.rescore_factor, "current_thread": self.current_thread,
        })
    }
}

enum Opened {
    A(LanceFlat),
    B(LanceIndexed),
    C(FjallFlat),
}

impl Opened {
    fn open(arm: Arm, dir: &Path, dim: usize, knobs: &Knobs) -> Result<Self> {
        Ok(match arm {
            Arm::A => Self::A(LanceFlat::open(dir, dim, knobs.current_thread)?),
            Arm::B => Self::B(LanceIndexed::open(
                dir,
                dim,
                knobs.current_thread,
                AnnParams {
                    exact_max_rows: knobs.exact_max_rows,
                    nprobes: knobs.nprobes,
                    refine_factor: knobs.refine_factor,
                    ef: knobs.ef,
                    partitions: knobs.partitions,
                    build_ann: knobs.ann,
                },
            )?),
            Arm::C => Self::C(FjallFlat::open(dir, dim, knobs.rescore_factor)?),
        })
    }

    fn backend(&mut self) -> &mut dyn Backend {
        match self {
            Self::A(arm) => arm,
            Self::B(arm) => arm,
            Self::C(arm) => arm,
        }
    }
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| match entry.metadata() {
                    Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

fn emit(mut line: Map<String, Value>, arm: Arm, params: &Params, knobs: &Knobs) {
    line.insert("arm".into(), json!(arm.name()));
    line.insert("n".into(), json!(params.n));
    line.insert("dim".into(), json!(params.dim));
    line.insert("label".into(), json!(knobs.label));
    line.insert("knobs".into(), knobs.json());
    println!("{}", Value::Object(line));
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn generate(n: u64, dim: usize, seed: u64, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let params = Params { n, dim, seed };
    let started = Instant::now();
    let corpus = Corpus::new(params);
    corpus.write_ipc(&dir.join("corpus.arrow"), 0, n)?;
    corpus.write_ipc(&dir.join("append.arrow"), n, params.append_rows())?;
    std::fs::write(dir.join("queries.json"), serde_json::to_vec(&corpus.queries())?)?;
    std::fs::write(dir.join("params.json"), serde_json::to_vec(&params)?)?;
    println!(
        "{}",
        json!({"kind": "gen", "n": n, "dim": dim, "seed": seed,
               "seconds": started.elapsed().as_secs_f64(), "disk_bytes": dir_size(dir)})
    );
    Ok(())
}

fn build(arm: Arm, stage: Stage, corpus_dir: &Path, store_dir: &Path, knobs: &Knobs) -> Result<()> {
    let params = Params::load(corpus_dir)?;
    let mut opened = Opened::open(arm, store_dir, params.dim, knobs)?;
    let started = Instant::now();
    let rows = |file: &str| -> Result<_> { Ok(open_ipc(&corpus_dir.join(file))?.map(|batch| Ok(batch?))) };
    let name = match stage {
        Stage::Ingest => {
            opened.backend().build(&mut rows("corpus.arrow")?)?;
            "ingest"
        }
        Stage::Index => {
            opened.backend().build_indexes()?;
            "index"
        }
        Stage::Append => {
            opened.backend().append(&mut rows("append.arrow")?)?;
            "append"
        }
        Stage::Optimize => {
            opened.backend().optimize()?;
            "optimize"
        }
    };
    let seconds = started.elapsed().as_secs_f64();
    let mut line = object(json!({
        "kind": "build", "stage": name, "seconds": seconds,
        "raw_vector_bytes": params.raw_vector_bytes(),
    }));
    if let Opened::B(indexed) = &opened {
        let per_index: Map<String, Value> = indexed
            .index_seconds
            .iter()
            .map(|(label, secs)| (label.to_string(), json!(secs)))
            .collect();
        line.insert("index_seconds".into(), Value::Object(per_index));
        let coverage: Vec<Value> = indexed
            .index_coverage()?
            .into_iter()
            .map(|(index, indexed, unindexed)| json!({"index": index, "indexed": indexed, "unindexed": unindexed}))
            .collect();
        line.insert("index_coverage".into(), json!(coverage));
    }
    drop(opened);
    line.insert("disk_bytes".into(), json!(dir_size(store_dir)));
    emit(line, arm, &params, knobs);
    Ok(())
}

struct LaneRun {
    /// Sorted latencies of the reported pass.
    latencies_ms: Vec<f64>,
    /// p95 of every pass, in run order.
    pass_p95_ms: Vec<f64>,
    recall: f64,
    ndcg: f64,
    exact: usize,
    ann: usize,
    short: usize,
}

/// Warm up, then time every query `TIMED_PASSES` times and score the answers against `gold`.
fn run_lane(
    gold: &[Vec<String>],
    mut answer: impl FnMut(usize) -> Result<(Vec<Hit>, Option<VectorMode>, bool)>,
) -> Result<LaneRun> {
    for at in 0..WARMUP_QUERIES.min(gold.len()) {
        answer(at)?;
    }
    let mut run = LaneRun {
        latencies_ms: Vec::new(),
        pass_p95_ms: Vec::new(),
        recall: 0.0,
        ndcg: 0.0,
        exact: 0,
        ann: 0,
        short: 0,
    };
    for _ in 1..TIMED_PASSES {
        let mut latencies = Vec::with_capacity(gold.len());
        for at in 0..gold.len() {
            let started = Instant::now();
            answer(at)?;
            latencies.push(started.elapsed().as_secs_f64() * 1e3);
        }
        run.keep_pass(latencies);
    }
    let mut latencies = Vec::with_capacity(gold.len());
    for (at, expected) in gold.iter().enumerate() {
        let started = Instant::now();
        let (hits, mode, short) = answer(at)?;
        latencies.push(started.elapsed().as_secs_f64() * 1e3);
        let (recall, ndcg) = quality(&hits, expected);
        run.recall += recall;
        run.ndcg += ndcg;
        match mode {
            Some(VectorMode::Exact) => run.exact += 1,
            Some(VectorMode::Ann) => run.ann += 1,
            None => {}
        }
        run.short += usize::from(short);
    }
    run.keep_pass(latencies);
    run.recall /= gold.len().max(1) as f64;
    run.ndcg /= gold.len().max(1) as f64;
    Ok(run)
}

impl LaneRun {
    fn keep_pass(&mut self, mut latencies: Vec<f64>) {
        latencies.sort_by(f64::total_cmp);
        let p95 = percentile(&latencies, 95.0);
        if self.pass_p95_ms.iter().all(|kept| p95 < *kept) {
            self.latencies_ms = latencies;
        }
        self.pass_p95_ms.push(p95);
    }
}

#[allow(clippy::too_many_arguments)] // ~keep one flat CLI subcommand maps onto one call
fn query(
    arm: Arm,
    corpus_dir: &Path,
    store_dir: &Path,
    selectivity: u32,
    live: u64,
    key_list: Option<usize>,
    state: &str,
    k: usize,
    knobs: &Knobs,
) -> Result<()> {
    let params = Params::load(corpus_dir)?;
    let queries = truth::load_queries(corpus_dir)?;
    let truth_state = if state == "base" { "base" } else { "appended" };
    let gold = Truth::load(corpus_dir, truth_state)?;
    let mut filter = Filter::for_selectivity(selectivity)?;
    let mut projected: Option<AHashSet<String>> = None;
    let mut live_mode = "none";
    let mut live_keys = 0;
    if let Some(count) = key_list {
        if live != 100 {
            bail!("--key-list replaces --live");
        }
        let docs = (gold.rows.div_ceil(CHUNKS_PER_DOC) as usize).min(count);
        let mut keys: Vec<String> = (0..docs as u64).map(|doc| doc_key(params.seed, doc)).collect();
        keys.extend((keys.len()..count).map(|i| doc_key(!params.seed, i as u64)));
        (live_mode, live_keys) = ("predicate", keys.len());
        filter = filter.with_keys(keys);
    } else if live < 100 {
        let keys = truth::live_keys(&params, gold.rows, live);
        live_keys = keys.len();
        if keys.len() <= MAX_PREDICATE_KEYS {
            live_mode = "predicate";
            filter = filter.with_keys(keys);
        } else {
            live_mode = "project";
            projected = Some(keys.into_iter().collect());
        }
    }
    let lookup = |lane: &str| -> Result<&Vec<Vec<String>>> {
        // A key-list run is timed, not scored; any entry supplies the query count.
        let name = variant(lane, selectivity, live);
        gold.entries
            .get(&name)
            .with_context(|| format!("truth has no entry {name}"))
    };

    let started = Instant::now();
    let mut opened = Opened::open(arm, store_dir, params.dim, knobs)?;
    let backend: &dyn Backend = opened.backend();
    let cold_open_ms = started.elapsed().as_secs_f64() * 1e3;
    let started = Instant::now();
    backend.vector(&queries[0].vector, &filter, k)?;
    let cold_vector_ms = started.elapsed().as_secs_f64() * 1e3;
    let started = Instant::now();
    let has_lexical = backend.lexical(&queries[0].text, &filter, k)?.is_some();
    let cold_lexical_ms = started.elapsed().as_secs_f64() * 1e3;

    let base = |lane: &str, run: &LaneRun| {
        object(json!({
            "kind": "query", "lane": lane, "selectivity": selectivity, "live": live, "state": state,
            "k": k, "queries": run.latencies_ms.len(),
            "live_mode": live_mode, "live_keys": live_keys, "key_list": key_list.unwrap_or(0),
            "recall10": key_list.is_none().then_some(run.recall),
            "ndcg10": key_list.is_none().then_some(run.ndcg),
            "p50_ms": percentile(&run.latencies_ms, 50.0), "p95_ms": percentile(&run.latencies_ms, 95.0),
            "pass_p95_ms": run.pass_p95_ms, "cold_open_ms": cold_open_ms,
        }))
    };

    let vector = run_lane(lookup("vector")?, |at| match &projected {
        Some(live_set) => {
            let (hits, mode, short) = truth::search_live(backend, &queries[at].vector, &filter, k, live_set)?;
            Ok((hits, Some(mode), short))
        }
        None => {
            let (hits, mode) = backend.vector(&queries[at].vector, &filter, k)?;
            Ok((hits, Some(mode), false))
        }
    })?;
    let mut line = base("vector", &vector);
    line.insert("cold_first_ms".into(), json!(cold_vector_ms));
    line.insert("exact_queries".into(), json!(vector.exact));
    line.insert("ann_queries".into(), json!(vector.ann));
    line.insert("liveness_short".into(), json!(vector.short));
    emit(line, arm, &params, knobs);

    if has_lexical && projected.is_none() {
        let lexical = run_lane(lookup("lexical")?, |at| {
            let hits = backend.lexical(&queries[at].text, &filter, k)?.unwrap_or_default();
            Ok((hits, None, false))
        })?;
        let mut line = base("lexical", &lexical);
        line.insert("cold_first_ms".into(), json!(cold_lexical_ms));
        emit(line, arm, &params, knobs);
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Gen {
            n,
            dim,
            seed,
            corpus_dir,
        } => generate(n, dim, seed, &corpus_dir),
        Command::Truth { corpus_dir, state } => {
            if state != "base" && state != "appended" {
                bail!("state must be base or appended");
            }
            let started = Instant::now();
            let computed = truth::compute(&corpus_dir, &state)?;
            let path = corpus_dir.join(format!("truth.{state}.json"));
            std::fs::write(&path, serde_json::to_vec(&computed)?)?;
            println!(
                "{}",
                json!({"kind": "truth", "state": state, "rows": computed.rows,
                       "seconds": started.elapsed().as_secs_f64()})
            );
            Ok(())
        }
        Command::Build {
            arm,
            stage,
            corpus_dir,
            store_dir,
            knobs,
        } => build(arm, stage, &corpus_dir, &store_dir, &knobs),
        Command::Query {
            arm,
            corpus_dir,
            store_dir,
            selectivity,
            live,
            key_list,
            state,
            k,
            knobs,
        } => query(
            arm,
            &corpus_dir,
            &store_dir,
            selectivity,
            live,
            key_list,
            &state,
            k,
            &knobs,
        ),
        Command::Probe { probe } => {
            let result = match probe {
                ProbeCommand::Nulls { scratch_dir } => probe::nulls(&scratch_dir)?,
                ProbeCommand::FtsNoIndex { scratch_dir } => probe::fts_no_index(&scratch_dir)?,
                ProbeCommand::Inlist {
                    corpus_dir,
                    store_dir,
                    keys,
                } => probe::inlist(&corpus_dir, &store_dir, keys)?,
            };
            println!("{result}");
            Ok(())
        }
    }
}
