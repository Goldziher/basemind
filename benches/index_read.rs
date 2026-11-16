//! Index read-path latency across the worktrees of one repository group.
//!
//! `benches/query.rs` covers the in-memory map cache. This covers the Fjall index behind
//! `code_refs`, the code-search exact lane and the grep bloom: it builds a synthetic repository
//! group (a git repository and its linked worktrees, see [`corpus`]), indexes every worktree
//! through `basemind::scanner::scan`, and times each read in [`ops`] at 1, 3 and 10 "content
//! versions", meaning the same paths indexed in that many worktrees whose files differ in a fixed
//! fraction.
//!
//! Each sample is one read against one worktree, taken round-robin over the group, and the report
//! is p50/p95 per operation and version count. Rows read per query are reported next to the
//! latency, with the rows a content-keyed group index would hold for the same name, so a layout
//! change that makes a read walk other worktrees' rows shows up in both.
//!
//! Not a criterion bench: criterion reports a mean and its confidence interval, and the gate this
//! baseline feeds is on p95. Run it with
//! `cargo bench --bench index_read -- [--quick] [--out <file.json>]`; see
//! `benchmarks/index/README.md`.

#[path = "index_read/corpus.rs"]
mod corpus;
#[path = "index_read/ops.rs"]
mod ops;

use std::collections::BTreeSet;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use corpus::{Expected, Lang, Params, WorktreeSpec};
use ops::{Definition, OpResult, Worktree};
use serde_json::{Value, json};

/// Page size of every paged read: the `code_refs` default.
const LIMIT: usize = 100;
/// Distinct targets a cold query cycles through. Prime, so it shares no factor with a version
/// count and every worktree sees every target.
const COLD_TARGETS: usize = 31;
const SEED: u64 = 0x5EED_1DE5;

struct Mode {
    name: &'static str,
    files: usize,
    versions: Vec<usize>,
    samples: usize,
    warmup: usize,
}

impl Mode {
    fn full() -> Self {
        Self {
            name: "full",
            files: 900,
            versions: vec![1, 3, 10],
            samples: 2000,
            warmup: 100,
        }
    }

    fn quick() -> Self {
        Self {
            name: "quick",
            files: 300,
            versions: vec![1, 3, 10],
            samples: 200,
            warmup: 20,
        }
    }

    /// What `cargo test --benches` runs: enough to prove the benchmark still builds its group and
    /// every operation still returns rows.
    fn smoke() -> Self {
        Self {
            name: "smoke",
            files: 120,
            versions: vec![1, 2],
            samples: 5,
            warmup: 1,
        }
    }
}

struct Args {
    mode: Mode,
    out: Option<PathBuf>,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| raw.iter().any(|arg| arg == flag);
    let value = |flag: &str| {
        raw.iter()
            .position(|arg| arg == flag)
            .and_then(|at| raw.get(at + 1))
            .cloned()
    };
    let mut mode = if !has("--bench") {
        Mode::smoke()
    } else if has("--quick") {
        Mode::quick()
    } else {
        Mode::full()
    };
    if let Some(files) = value("--files") {
        mode.files = files.parse().expect("--files takes a number");
    }
    if let Some(samples) = value("--samples") {
        mode.samples = samples.parse().expect("--samples takes a number");
    }
    if let Some(versions) = value("--versions") {
        mode.versions = versions
            .split(',')
            .map(|v| v.parse().expect("--versions takes numbers separated by commas"))
            .collect();
    }
    Args {
        mode,
        out: value("--out").map(PathBuf::from),
    }
}

/// Point every basemind path at `base`, so the run neither reads nor writes the user's cache and
/// cannot reach a running daemon.
fn isolate(base: &Path) {
    // SAFETY: called first in `main`, before the process has started another thread.
    unsafe {
        std::env::set_var("BASEMIND_DATA_HOME", base.join("data"));
        std::env::set_var("BASEMIND_COMMS_DIR", base.join("comms"));
        // A sibling's index is never cloned into a new worktree: every index here is built by its
        // own scan, whatever order the worktrees are opened in.
        std::env::set_var("BASEMIND_NO_SEED", "1");
    }
}

struct Group {
    specs: Vec<WorktreeSpec>,
    worktrees: Vec<Worktree>,
    /// Per worktree: the definition of `dispatch_event` in the Rust core file.
    hot_definitions: Vec<Definition>,
    /// Per worktree, per cold target: a file-local helper.
    cold_definitions: Vec<Vec<Definition>>,
    index_secs: f64,
}

/// File index of cold target `target`.
fn cold_file(params: &Params, target: usize) -> usize {
    (target * 7919 + 13) % params.files
}

impl Group {
    fn build(dir: &Path, params: &Params, versions: usize) -> Self {
        let specs = corpus::build_group(dir, params, versions);
        let started = Instant::now();
        let worktrees: Vec<Worktree> = specs.iter().map(|spec| Worktree::index(&spec.root)).collect();
        let index_secs = started.elapsed().as_secs_f64();
        let hot_path = corpus::core_path(Lang::Rust);
        let hot_definitions = worktrees
            .iter()
            .map(|worktree| worktree.definition(&hot_path, "dispatch_event"))
            .collect();
        let cold_definitions = worktrees
            .iter()
            .map(|worktree| {
                (0..COLD_TARGETS)
                    .map(|target| {
                        let file = cold_file(params, target);
                        worktree.definition(&corpus::file_path(file), &corpus::helper_name(file, 0))
                    })
                    .collect()
            })
            .collect();
        Self {
            specs,
            worktrees,
            hot_definitions,
            cold_definitions,
            index_secs,
        }
    }

    fn versions(&self) -> usize {
        self.worktrees.len()
    }

    fn distinct_contents(&self) -> usize {
        self.worktrees
            .iter()
            .flat_map(|worktree| worktree.content_hashes().map(|(_, hash)| hash))
            .collect::<BTreeSet<&str>>()
            .len()
    }

    /// `calls_by_callee` rows matching `name`: in one worktree's index, summed over the group's
    /// indexes, and counted once per distinct file content, which is what one content-keyed index
    /// for the group would hold.
    fn callee_row_counts(&self, name: &str) -> (usize, usize, usize) {
        let mut one = 0;
        let mut sum = 0;
        let mut distinct: BTreeSet<(String, u32)> = BTreeSet::new();
        for (at, worktree) in self.worktrees.iter().enumerate() {
            let hash_of: ahash::AHashMap<_, _> = worktree.content_hashes().collect();
            let rows = worktree.callee_rows(name);
            if at == 0 {
                one = rows.len();
            }
            sum += rows.len();
            for (rel, start) in rows {
                let hash = hash_of.get(&rel).expect("a call row names an indexed file");
                distinct.insert(((*hash).to_string(), start));
            }
        }
        (one, sum, distinct.len())
    }

    fn index_bytes(&self) -> u64 {
        self.worktrees
            .iter()
            .map(|worktree| dir_bytes(&worktree.cache_dir()))
            .sum()
    }
}

/// Allocated bytes under `dir`. Fjall preallocates its journal as a sparse file, so the logical
/// length over-reports it.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_bytes(&entry.path()),
            Ok(meta) => allocated(&meta),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(unix)]
fn allocated(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn allocated(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

#[derive(Clone, Copy)]
enum Op {
    ReferencesSubstringHot,
    ReferencesExactHot,
    ReferencesExactWarm,
    ReferencesExactCold,
    CallersHot,
    CallersCold,
    ImplementationsHot,
    ImplementationsWarm,
    SymbolExactHot,
    SymbolExactCold,
    GrepBloomRare,
    GrepBloomAbsent,
}

impl Op {
    const ALL: [Op; 12] = [
        Op::ReferencesSubstringHot,
        Op::ReferencesExactHot,
        Op::ReferencesExactWarm,
        Op::ReferencesExactCold,
        Op::CallersHot,
        Op::CallersCold,
        Op::ImplementationsHot,
        Op::ImplementationsWarm,
        Op::SymbolExactHot,
        Op::SymbolExactCold,
        Op::GrepBloomRare,
        Op::GrepBloomAbsent,
    ];

    fn id(self) -> &'static str {
        match self {
            Op::ReferencesSubstringHot => "references/substring/hot",
            Op::ReferencesExactHot => "references/exact/hot",
            Op::ReferencesExactWarm => "references/exact/warm",
            Op::ReferencesExactCold => "references/exact/cold",
            Op::CallersHot => "callers/hot",
            Op::CallersCold => "callers/cold",
            Op::ImplementationsHot => "implementations/hot",
            Op::ImplementationsWarm => "implementations/warm",
            Op::SymbolExactHot => "symbol_exact/hot",
            Op::SymbolExactCold => "symbol_exact/cold",
            Op::GrepBloomRare => "grep_bloom/rare",
            Op::GrepBloomAbsent => "grep_bloom/absent",
        }
    }

    fn query(self) -> String {
        match self {
            Op::ReferencesSubstringHot | Op::ReferencesExactHot => "get".to_string(),
            Op::ReferencesExactWarm => "record_metric".to_string(),
            Op::ReferencesExactCold => format!("helper_<file>_0 ({COLD_TARGETS} targets)"),
            Op::CallersHot => format!("dispatch_event in {}", corpus::core_path(Lang::Rust)),
            Op::CallersCold => format!("helper_<file>_0 in its file ({COLD_TARGETS} targets)"),
            Op::ImplementationsHot => corpus::HOT_TRAIT.to_string(),
            Op::ImplementationsWarm => corpus::WARM_TRAIT.to_string(),
            Op::SymbolExactHot => corpus::HOT_SYMBOL.to_string(),
            Op::SymbolExactCold => format!("build_widget_<file> ({COLD_TARGETS} targets)"),
            Op::GrepBloomRare => corpus::RARE_LITERAL.to_string(),
            Op::GrepBloomAbsent => corpus::ABSENT_LITERAL.to_string(),
        }
    }

    /// Sample `sample` of this operation: worktrees round-robin, cold targets cycling under them.
    fn run(self, group: &Group, params: &Params, sample: usize) -> OpResult {
        let at = sample % group.versions();
        let worktree = &group.worktrees[at];
        let target = sample % COLD_TARGETS;
        let file = cold_file(params, target);
        match self {
            Op::ReferencesSubstringHot => worktree.references("get", false, LIMIT),
            Op::ReferencesExactHot => worktree.references("get", true, LIMIT),
            Op::ReferencesExactWarm => worktree.references("record_metric", true, LIMIT),
            Op::ReferencesExactCold => worktree.references(&corpus::helper_name(file, 0), true, LIMIT),
            Op::CallersHot => worktree.callers(&group.hot_definitions[at], LIMIT),
            Op::CallersCold => worktree.callers(&group.cold_definitions[at][target], LIMIT),
            Op::ImplementationsHot => worktree.implementations(corpus::HOT_TRAIT, LIMIT),
            Op::ImplementationsWarm => worktree.implementations(corpus::WARM_TRAIT, LIMIT),
            Op::SymbolExactHot => worktree.symbol_exact(corpus::HOT_SYMBOL, LIMIT),
            Op::SymbolExactCold => worktree.symbol_exact(&corpus::builder_name(file), LIMIT),
            Op::GrepBloomRare => worktree.grep_candidates(corpus::RARE_LITERAL),
            Op::GrepBloomAbsent => worktree.grep_candidates(corpus::ABSENT_LITERAL),
        }
    }
}

/// Fail unless the index returns what the generator wrote. A read that examined nothing is as fast
/// as one that worked, so the timings are only taken on a group that passes this.
fn verify(group: &Group, params: &Params) {
    for (at, (worktree, spec)) in group.worktrees.iter().zip(&group.specs).enumerate() {
        let expected: &Expected = &spec.expected;
        let label = format!("worktree {at} of {}", group.versions());
        assert_eq!(
            worktree.file_count(),
            spec.paths.len() + 3,
            "{label}: indexed file count"
        );
        for name in ["dispatch_event", "acquire_lock", "record_metric"] {
            assert_eq!(
                worktree.callee_rows(name).len(),
                expected.calls[name],
                "{label}: `{name}` call rows against call sites generated"
            );
        }
        let warm = worktree.references("record_metric", true, LIMIT);
        assert_eq!(
            warm.total, expected.calls["record_metric"],
            "{label}: exact `record_metric`"
        );
        assert_eq!(warm.hits, LIMIT.min(warm.total), "{label}: `record_metric` page");
        let get = worktree.references("get", true, LIMIT);
        let substring = worktree.references("get", false, LIMIT);
        assert_eq!(get.hits, LIMIT, "{label}: exact `get` fills a page");
        assert!(
            substring.total >= get.total,
            "{label}: a substring scan matches at least what the exact scan does"
        );
        assert_eq!(
            worktree.implementations(corpus::HOT_TRAIT, LIMIT).total,
            expected.hot_trait_impls.min(2_000),
            "{label}: `{}` implementations",
            corpus::HOT_TRAIT
        );
        assert_eq!(
            worktree.implementations(corpus::WARM_TRAIT, LIMIT).total,
            expected.warm_trait_impls,
            "{label}: `{}` implementations",
            corpus::WARM_TRAIT
        );
        assert_eq!(
            worktree.symbol_exact(corpus::HOT_SYMBOL, LIMIT).hits,
            LIMIT.min(params.files),
            "{label}: `{}` definitions",
            corpus::HOT_SYMBOL
        );
        let rare = worktree.grep_candidates(corpus::RARE_LITERAL);
        assert!(
            rare.hits >= expected.rare_literal_files && rare.hits < rare.total / 2,
            "{label}: the bloom keeps the {} files holding the rare literal and skips most others, got {} of {}",
            expected.rare_literal_files,
            rare.hits,
            rare.total
        );
        let absent = worktree.grep_candidates(corpus::ABSENT_LITERAL);
        assert!(
            absent.hits < absent.total / 10,
            "{label}: the bloom skips nearly every file for an absent literal, got {} of {}",
            absent.hits,
            absent.total
        );
        for (target, definition) in group.cold_definitions[at].iter().enumerate() {
            let file = cold_file(params, target);
            let cold = worktree.references(&corpus::helper_name(file, 0), true, LIMIT);
            let callers = worktree.callers(definition, LIMIT);
            assert!(
                callers.rows >= cold.rows,
                "{label}: callers reads at least the name scan"
            );
            assert_eq!(
                worktree.symbol_exact(&corpus::builder_name(file), LIMIT).hits,
                1,
                "{label}: one definition of {}",
                corpus::builder_name(file)
            );
        }
        let hot = worktree.callers(&group.hot_definitions[at], LIMIT);
        assert_eq!(hot.hits, LIMIT, "{label}: callers of `dispatch_event` fill a page");
    }
}

struct Timing {
    op: Op,
    p50_ns: u64,
    p95_ns: u64,
    mean_ns: u64,
    min_ns: u64,
    samples: usize,
    mean_rows: f64,
    mean_hits: f64,
}

fn time_op(op: Op, group: &Group, params: &Params, mode: &Mode) -> Timing {
    for sample in 0..mode.warmup {
        black_box(op.run(group, params, sample));
    }
    let mut nanos: Vec<u64> = Vec::with_capacity(mode.samples);
    let mut rows = 0usize;
    let mut hits = 0usize;
    for sample in 0..mode.samples {
        let started = Instant::now();
        let result = black_box(op.run(group, params, sample));
        nanos.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        rows += result.rows;
        hits += result.hits;
    }
    nanos.sort_unstable();
    let count = nanos.len();
    Timing {
        op,
        p50_ns: nanos[count / 2],
        p95_ns: nanos[(count * 95 / 100).min(count - 1)],
        mean_ns: nanos.iter().sum::<u64>() / count as u64,
        min_ns: nanos[0],
        samples: count,
        mean_rows: rows as f64 / count as f64,
        mean_hits: hits as f64 / count as f64,
    }
}

fn micros(nanos: u64) -> f64 {
    nanos as f64 / 1000.0
}

fn measure_group(base: &Path, params: &Params, mode: &Mode, versions: usize) -> Value {
    let dir = base.join(format!("group-{versions}x"));
    let group = Group::build(&dir, params, versions);
    verify(&group, params);

    let paths = group.worktrees[0].file_count();
    let distinct = group.distinct_contents();
    let changed: Vec<usize> = group.specs.iter().map(|spec| spec.changed).collect();
    let index_bytes = group.index_bytes();
    println!(
        "\n== {versions}x: {} worktrees x {paths} paths, {distinct} distinct contents, indexed in {:.1}s, {:.1} MiB of index",
        group.versions(),
        group.index_secs,
        index_bytes as f64 / (1024.0 * 1024.0),
    );

    let mut row_counts = serde_json::Map::new();
    for name in ["get", "dispatch_event", "record_metric"] {
        let (one, sum, by_content) = group.callee_row_counts(name);
        println!(
            "   calls_by_callee rows containing `{name}`: {one} in one index, {sum} over the group, {by_content} by distinct content"
        );
        row_counts.insert(
            name.to_string(),
            json!({ "one_index": one, "group_sum": sum, "distinct_content": by_content }),
        );
    }

    println!(
        "   {:<28} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "operation", "p50 us", "p95 us", "mean us", "rows/read", "hits/read"
    );
    let mut ops = Vec::new();
    for op in Op::ALL {
        let timing = time_op(op, &group, params, mode);
        println!(
            "   {:<28} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>9.1}",
            timing.op.id(),
            micros(timing.p50_ns),
            micros(timing.p95_ns),
            micros(timing.mean_ns),
            timing.mean_rows,
            timing.mean_hits,
        );
        ops.push(json!({
            "op": timing.op.id(),
            "query": timing.op.query(),
            "p50_us": micros(timing.p50_ns),
            "p95_us": micros(timing.p95_ns),
            "mean_us": micros(timing.mean_ns),
            "min_us": micros(timing.min_ns),
            "samples": timing.samples,
            "rows_per_read": timing.mean_rows,
            "hits_per_read": timing.mean_hits,
        }));
    }

    let record = json!({
        "versions": versions,
        "worktrees": group.versions(),
        "paths_per_worktree": paths,
        "changed_files_per_worktree": changed,
        "distinct_contents": distinct,
        "index_seconds": group.index_secs,
        "index_bytes": index_bytes,
        "calls_by_callee_rows": row_counts,
        "ops": ops,
    });
    drop(group);
    // Each group holds a full index per worktree; remove it before building the next.
    let _ = std::fs::remove_dir_all(&dir);
    record
}

fn main() {
    let args = parse_args();
    let mode = args.mode;
    let base = tempfile::tempdir().expect("tempdir");
    isolate(base.path());
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");

    let params = Params {
        files: mode.files,
        helpers_per_file: 6,
        calls_per_fn: 5,
        changed_per_mille: 100,
        seed: SEED,
    };
    println!(
        "index_read ({}): {} files per worktree, {} samples per operation, versions {:?}",
        mode.name, mode.files, mode.samples, mode.versions
    );

    let groups: Vec<Value> = mode
        .versions
        .iter()
        .map(|versions| measure_group(base.path(), &params, &mode, *versions))
        .collect();

    let report = json!({
        "bench": "index_read",
        "schema": 1,
        "layout": "one index per worktree",
        "mode": mode.name,
        "unix_time": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
        "basemind_version": env!("CARGO_PKG_VERSION"),
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cpus": std::thread::available_parallelism().map_or(0, usize::from),
        },
        "params": {
            "files_per_worktree": params.files,
            "languages": ["rust", "typescript", "python"],
            "helpers_per_file": params.helpers_per_file,
            "calls_per_fn": params.calls_per_fn,
            "hot_callees_per_mille": corpus::HOT_CALLEES
                .iter()
                .map(|(name, share)| json!({ "name": name, "share": share }))
                .collect::<Vec<_>>(),
            "changed_per_mille": params.changed_per_mille,
            "seed": params.seed,
            "page_limit": LIMIT,
            "cold_targets": COLD_TARGETS,
            "samples": mode.samples,
            "warmup": mode.warmup,
        },
        "groups": groups,
    });
    if let Some(out) = args.out {
        let text = serde_json::to_string_pretty(&report).expect("serialise the report");
        std::fs::write(&out, text + "\n").expect("write the report");
        println!("\nwrote {}", out.display());
    }
}
