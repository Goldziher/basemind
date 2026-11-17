//! Shared embedding engine for the memory + documents MCP tools.

use std::sync::{Mutex, OnceLock, PoisonError};

use anyhow::{Context, Result, anyhow};
use xberg::embeddings::EMBEDDING_PRESETS;
use xberg::{EmbeddingConfig, EmbeddingModelType};

use crate::config::OnnxProvider;

/// The identity of `preset`'s model, stamped on every stored vector and naming the vector stores:
/// the preset name, then a digest of what xberg loads for it (repository, file, sibling files,
/// pooling, dimension, backend, query prefix). An xberg release that points a preset name at
/// another model changes the digest, so vectors of the old model are re-embedded instead of being
/// mixed with the new one's. An unknown preset is its own name.
pub fn model_key(preset: &str) -> std::borrow::Cow<'static, str> {
    static KEYS: OnceLock<ahash::AHashMap<String, String>> = OnceLock::new();
    let keys = KEYS.get_or_init(|| {
        EMBEDDING_PRESETS
            .iter()
            .map(|p| {
                let identity = format!(
                    "{}\0{}\0{:?}\0{}\0{}\0{:?}\0{:?}",
                    p.model_repo, p.model_file, p.additional_files, p.pooling, p.dimensions, p.backend, p.query_prefix
                );
                let digest = crate::hashing::hash_bytes(identity.as_bytes());
                let hex = crate::hashing::hex_buf(&digest);
                (
                    p.name.clone(),
                    format!("{}@{}", p.name, &crate::hashing::hex_str(&hex)[..12]),
                )
            })
            .collect()
    });
    match keys.get(preset) {
        Some(key) => std::borrow::Cow::Borrowed(key.as_str()),
        None => std::borrow::Cow::Owned(preset.to_string()),
    }
}

/// Translate `[resources] onnx_provider` into xberg's acceleration setting. `Auto` yields `None`, so
/// xberg applies its own platform default.
pub fn acceleration(provider: OnnxProvider) -> Option<xberg::AccelerationConfig> {
    use xberg::core::config::ExecutionProviderType as Provider;
    let provider = match provider {
        OnnxProvider::Auto => return None,
        OnnxProvider::Cpu => Provider::Cpu,
        OnnxProvider::CoreMl => Provider::CoreMl,
        OnnxProvider::Cuda => Provider::Cuda,
        OnnxProvider::TensorRt => Provider::TensorRt,
    };
    Some(xberg::AccelerationConfig { provider, device_id: 0 })
}

/// Bound ONNX Runtime's memory for every session built from now on: no memory-pattern planning, no
/// retaining CPU arena, and at most `intra_threads` intra-op threads. ORT's defaults keep allocations
/// sized to the largest batch seen and never shrink, so one embedding pass over variable-length
/// documents grew the daemon by ~2.7 GB. Trades some throughput for a bounded footprint.
///
/// Call before the first embedding; resident engines keep the options they were built with. Only the
/// first call in a process takes effect (changing options clears the engine caches, which must not
/// happen under a live session), so an entry point that knows its mode calls this first and the
/// generic CLI default never overrides it.
pub fn bound_ort_memory(intra_threads: usize) {
    static APPLIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if APPLIED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    xberg::set_ort_session_options(xberg::OrtSessionOptions {
        memory_pattern: false,
        cpu_arena: false,
        max_threads: Some(intra_threads),
    });
}

/// Drop every resident embedding engine so its ONNX session, weights and arena are freed. The next
/// embedding reloads the model. Returns how many engines were dropped.
pub fn release_engines() -> usize {
    xberg::clear_engine_caches()
}

static EMBED_PASSES_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Serializes [`begin_embed_pass`] against the last pass's release. Without it, a new pass could
/// increment the counter in the window between the final decrement and [`release_engines`], and
/// have the engines it is about to use freed underneath it. Held only across the counter update
/// (and the release), never across the embedding work itself.
static PASS_GATE: Mutex<()> = Mutex::new(());

/// RAII marker for one embedding pass in this process. When the LAST concurrent pass ends, resident
/// engines are released, so a pass for one workspace never frees the model out from under (or forces a
/// reload for) a pass that is still running for another.
#[must_use = "the engines are released when this guard drops"]
pub struct EmbedPass(());

/// Register the start of an embedding pass; see [`EmbedPass`].
pub fn begin_embed_pass() -> EmbedPass {
    let _gate = PASS_GATE.lock().unwrap_or_else(PoisonError::into_inner);
    EMBED_PASSES_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    EmbedPass(())
}

impl Drop for EmbedPass {
    fn drop(&mut self) {
        let _gate = PASS_GATE.lock().unwrap_or_else(PoisonError::into_inner);
        if EMBED_PASSES_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            tracing::info!(
                dropped = release_engines(),
                "embedding pass finished; released resident embedding engines"
            );
        }
    }
}

/// Number of embedding passes currently in flight. Exposed for tests.
pub fn embed_passes_in_flight() -> usize {
    EMBED_PASSES_IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire)
}

/// Global bounded rayon `ThreadPool` for all ONNX embed calls. Initialized once
/// on first use; subsequent calls to `embed_pool` return the same pool regardless
/// of the `max_threads` argument (the pool size is fixed for the process).
static EMBED_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// Resolve the embedding thread cap.
///
/// `0` is the sentinel for "auto": `max(2, logical_cpus / 4)` — a bounded
/// fraction of available cores that leaves the full global rayon pool free for
/// code-map extraction and prevents the embedder from pinning all cores.
/// Any non-zero value is used directly.
pub fn resolve_embed_threads(max_threads: usize) -> usize {
    if max_threads == 0 {
        std::cmp::max(2, rayon::current_num_threads() / 4)
    } else {
        max_threads
    }
}

/// Returns the process-wide bounded rayon `ThreadPool` for ONNX embedding.
///
/// Initialized once on first call with `resolve_embed_threads(max_threads)`.
/// All `embed_texts` calls from [`SharedEmbedder`] run inside this pool via
/// `pool.install(...)`, which constrains xberg's internal rayon tasks (including
/// the per-chunk embedding fan-out) to at most `current_num_threads()` workers.
pub fn embed_pool(max_threads: usize) -> &'static rayon::ThreadPool {
    EMBED_POOL.get_or_init(|| {
        let n = resolve_embed_threads(max_threads);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("bm-embed-{i}"))
            .build()
            .expect("failed to build embedding rayon pool")
    })
}

/// What stands in for the model in a test: the vector for a text.
#[cfg(test)]
pub(crate) type FakeEmbed = fn(&str) -> Result<Vec<f32>>;

/// Vector length of `preset`, or the error [`SharedEmbedder::load`] gives for it: the preset table
/// lookup alone, with no model or config built.
pub(crate) fn preset_dimension(preset: &str) -> Result<u16> {
    let meta = EMBEDDING_PRESETS.iter().find(|p| p.name == preset).ok_or_else(|| {
        anyhow!(
            "unknown embedding preset '{preset}'; \
                 available: fast, balanced, quality, multilingual"
        )
    })?;
    u16::try_from(meta.dimensions)
        .with_context(|| format!("preset '{preset}' dimension {} exceeds u16", meta.dimensions))
}

/// The scan's [`Embedder`](crate::scan_retrieval::Embedder). `dim` reads the preset table;
/// `embed_batch` loads a [`SharedEmbedder`] and embeds through it.
#[cfg(feature = "code-search")]
pub(crate) struct ScanEmbedder;

#[cfg(feature = "code-search")]
impl crate::scan_retrieval::Embedder for ScanEmbedder {
    fn dim(
        &self,
        spec: &crate::scan_retrieval::EmbedderSpec<'_>,
    ) -> Result<u16, crate::scan_retrieval::RetrievalError> {
        preset_dimension(spec.preset).map_err(|error| crate::scan_retrieval::RetrievalError(format!("{error:#}")))
    }

    fn embed_batch(
        &self,
        spec: &crate::scan_retrieval::EmbedderSpec<'_>,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, crate::scan_retrieval::RetrievalError> {
        SharedEmbedder::load_with_provider(spec.preset, spec.max_threads, spec.batch_size, spec.provider)
            .and_then(|embedder| embedder.embed_batch(texts))
            .map_err(|error| crate::scan_retrieval::RetrievalError(format!("{error:#}")))
    }
}

/// Loaded, ready-to-query embedding engine. `Clone` is cheap (config is stack-only).
#[derive(Clone)]
pub struct SharedEmbedder {
    config: EmbeddingConfig,
    dim: u16,
    model_name: String,
    /// Resolved embed-thread cap passed through to `embed_pool`. `0` triggers the
    /// auto heuristic inside `resolve_embed_threads`; first caller wins for the
    /// global pool.
    max_embed_threads: usize,
    /// A stand-in for the model: tests exercise the callers of an embedder without loading one.
    #[cfg(test)]
    fake: Option<FakeEmbed>,
}

impl SharedEmbedder {
    /// An embedder that reports `model` and `dim` and answers every text with `embed`.
    #[cfg(all(test, feature = "memory"))]
    pub(crate) fn fake(model: &str, dim: u16, embed: FakeEmbed) -> Self {
        let mut embedder = Self::load("fast", 1, 8).expect("the fast preset exists");
        embedder.dim = dim;
        embedder.model_name = model.to_string();
        embedder.fake = Some(embed);
        embedder
    }

    /// Build a `SharedEmbedder` from a named xberg preset.
    ///
    /// `max_embed_threads` bounds the process-wide embedding pool (see
    /// [`embed_pool`]). Pass `0` to use the auto heuristic (`max(2, cores/4)`).
    ///
    /// `batch_size` is the number of texts submitted to ONNX per embed call
    /// (sourced from `[resources].embed_batch_size`). Larger batches amortise
    /// per-call overhead at a higher transient memory spike.
    pub fn load(preset: &str, max_embed_threads: usize, batch_size: usize) -> Result<Self> {
        Self::load_with_provider(preset, max_embed_threads, batch_size, OnnxProvider::default())
    }

    /// [`load`](Self::load) with an explicit ONNX execution provider (`[resources] onnx_provider`).
    pub fn load_with_provider(
        preset: &str,
        max_embed_threads: usize,
        batch_size: usize,
        provider: OnnxProvider,
    ) -> Result<Self> {
        let dim = preset_dimension(preset)?;
        let config = EmbeddingConfig {
            model: EmbeddingModelType::Preset {
                name: preset.to_string(),
            },
            normalize: true,
            batch_size,
            show_download_progress: false,
            cache_dir: None,
            acceleration: acceleration(provider),
            max_embed_duration_secs: Some(60),
            max_sequence_length: None,
        };
        Ok(Self {
            config,
            dim,
            model_name: preset.to_string(),
            max_embed_threads,
            #[cfg(test)]
            fake: None,
        })
    }

    /// Vector dimension produced by this embedder.
    pub fn dim(&self) -> u16 {
        self.dim
    }

    /// The preset name (e.g. `"balanced"`).
    pub fn model(&self) -> &str {
        &self.model_name
    }

    /// Embed a single text string. Returns a `Vec<f32>` of length `self.dim()`.
    ///
    /// The call is routed through the process-wide bounded [`embed_pool`] so it
    /// cannot saturate the global rayon pool used by the code-map scanner.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Err(anyhow!("embed: input text must not be empty"));
        }
        #[cfg(test)]
        if let Some(fake) = self.fake {
            return fake(text);
        }
        load_failure::recent(&self.model_name)?;
        let (config, model, text) = (self.config.clone(), self.model_name.clone(), text.to_string());
        load_failure::remember(
            &self.model_name,
            self.run_on_embed_pool(move || {
                let mut results = xberg::embeddings::embed_texts(&[text.as_str()], &config)
                    .with_context(|| format!("embed_texts(preset={model})"))?;
                results
                    .pop()
                    .ok_or_else(|| anyhow!("embed_texts returned empty result"))
            })?,
        )
    }

    /// Embed a search query: [`Self::embed`] with the preset's query prefix in front, which
    /// asymmetric models need to put a query and the passages it should find in one space (for
    /// example `arctic-embed-m-v2.0` wants `query: `). Passages are embedded without it.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        self.embed(&with_query_prefix(self.query_prefix().as_deref(), query))
    }

    /// The preset's query prefix, if it has one.
    pub fn query_prefix(&self) -> Option<String> {
        xberg::embeddings::embedding_query_prefix(&self.config).filter(|prefix| !prefix.is_empty())
    }

    /// Embed a batch of texts in one call. Returns one `Vec<f32>` of length `self.dim()` per
    /// input, in order.
    ///
    /// Errors if any input text is empty (xberg rejects empty strings, which produce meaningless
    /// embeddings). An empty batch returns an empty vector without touching the model.
    ///
    /// The call is routed through the process-wide bounded [`embed_pool`] so it
    /// cannot saturate the global rayon pool used by the code-map scanner.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(test)]
        if let Some(fake) = self.fake {
            return texts.iter().map(|text| fake(text)).collect();
        }
        load_failure::recent(&self.model_name)?;
        let (config, model) = (self.config.clone(), self.model_name.clone());
        let owned: Vec<String> = texts.iter().map(|t| (*t).to_string()).collect();
        load_failure::remember(
            &self.model_name,
            self.run_on_embed_pool(move || {
                let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
                xberg::embeddings::embed_texts(&refs, &config)
                    .with_context(|| format!("embed_texts(preset={model}, batch={})", refs.len()))
            })?,
        )
    }

    /// Run `work` on the bounded [`embed_pool`] and block the caller until it finishes.
    ///
    /// Not `ThreadPool::install`: when the caller is a worker of a *different* rayon pool (the scan
    /// pool), `install` parks it in rayon's wait loop, which keeps executing queued jobs from the
    /// caller's own pool while it waits. A scan worker holding a document slot then picked up another
    /// document, blocked on the same slot, and the scan deadlocked (`max_concurrent_documents = 1`
    /// with one scan thread hung forever; with more threads every slot ends up held by a blocked
    /// frame). A plain channel wait never runs foreign work.
    fn run_on_embed_pool<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let pool = embed_pool(self.max_embed_threads);
        if pool.current_thread_index().is_some() {
            // Already on an embed worker: blocking it on its own pool could starve the pool.
            return Ok(work());
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        // rayon aborts the whole process when a `spawn`ed closure unwinds (no panic handler is
        // installed), so the panic must be contained here and reported through the channel.
        pool.spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)));
        });
        match rx.recv() {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) | Err(_) => Err(anyhow!("embedding worker panicked")),
        }
    }
}

/// `query` with `prefix` in front, when there is one and the query does not already carry it.
pub fn with_query_prefix(prefix: Option<&str>, query: &str) -> String {
    match prefix {
        Some(prefix) if !query.starts_with(prefix) => format!("{prefix}{query}"),
        _ => query.to_string(),
    }
}

/// A model that could not be loaded (offline, a failed download, no ONNX runtime) is not tried
/// again by every call for a while: each attempt can take as long as the download timeout.
/// A stand-in for the document embedder, settable by integration tests (the `test-support`
/// feature): `Some(f)` makes every document embedding call answer with `f(texts, dim)`, so a test
/// can make the embedder fail, then work, without a model.
#[cfg(feature = "test-support")]
pub mod test_document_embedder {
    use std::sync::RwLock;

    /// Embeds `texts` at `dim`, or fails with a reason.
    pub type Embed = fn(&[&str], u16) -> Result<Vec<Vec<f32>>, String>;

    static OVERRIDE: RwLock<Option<Embed>> = RwLock::new(None);

    /// Install (or with `None` remove) the stand-in.
    pub fn set(embed: Option<Embed>) {
        *OVERRIDE.write().unwrap_or_else(std::sync::PoisonError::into_inner) = embed;
    }

    pub(crate) fn get() -> Option<Embed> {
        *OVERRIDE.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

mod load_failure {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow};

    /// How long a load failure is answered from memory before the model is tried again.
    pub(super) const RETRY_AFTER: Duration = Duration::from_secs(60);

    static FAILED: Mutex<Option<(String, Instant, String)>> = Mutex::new(None);

    /// The remembered failure of `model`, while it is recent.
    pub(super) fn recent(model: &str) -> Result<()> {
        let failed = FAILED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match failed.as_ref() {
            Some((name, at, reason)) if name == model && at.elapsed() < RETRY_AFTER => Err(anyhow!(
                "{reason} (a load failure less than {} s ago; not retried yet)",
                RETRY_AFTER.as_secs()
            )),
            _ => Ok(()),
        }
    }

    /// Pass `result` through, remembering it when it failed to load `model`, forgetting any earlier
    /// failure when it succeeded.
    pub(super) fn remember<T>(model: &str, result: Result<T>) -> Result<T> {
        let mut failed = FAILED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match &result {
            Ok(_) => *failed = None,
            Err(error) if is_load_failure(&format!("{error:#}")) => {
                *failed = Some((model.to_string(), Instant::now(), format!("{error:#}")));
            }
            Err(_) => {}
        }
        result
    }

    /// Whether an embedding error is the model failing to load rather than one input failing.
    pub(super) fn is_load_failure(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        [
            "download",
            "huggingface",
            "onnx",
            "missing dependency",
            "no such file",
            "network",
            "connect",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_pass_guards_nest_and_count_down() {
        let base = embed_passes_in_flight();
        let first = begin_embed_pass();
        let second = begin_embed_pass();
        assert!(embed_passes_in_flight() >= base + 2);
        drop(first);
        assert!(
            embed_passes_in_flight() > base,
            "a pass is still running after the first ends"
        );
        drop(second);
    }

    /// The scan deadlock: a scan worker holding a document slot blocked on the embed pool via
    /// `ThreadPool::install`, which parks a rayon worker in a loop that executes other queued jobs of
    /// ITS pool. It picked up a second document, which blocked on the slot the first frame held.
    /// One scan thread and one slot reproduce it deterministically: with `install` this test hangs.
    #[test]
    fn waiting_on_the_embed_pool_never_runs_foreign_work_on_the_waiting_worker() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;

        let embedder = SharedEmbedder::load("fast", 2, 8).expect("fast preset");
        let scan_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("scan pool");
        let slot = Arc::new(Mutex::new(()));
        let (done_tx, done_rx) = mpsc::channel();
        let (first_in_tx, first_in_rx) = mpsc::channel();

        let (holder_slot, holder_embedder, holder_done) = (Arc::clone(&slot), embedder.clone(), done_tx.clone());
        scan_pool.spawn(move || {
            let _held = holder_slot.lock().unwrap();
            first_in_tx.send(()).ok();
            // Give the second job time to be queued behind us, then wait on the embed pool.
            std::thread::sleep(Duration::from_millis(50));
            let answer = holder_embedder.run_on_embed_pool(|| 7).expect("embed pool result");
            holder_done.send(("first", answer)).ok();
        });
        first_in_rx.recv().expect("first job started");
        let second_slot = Arc::clone(&slot);
        scan_pool.spawn(move || {
            let _held = second_slot.lock().unwrap();
            done_tx.send(("second", 0)).ok();
        });

        let mut finished = vec![
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("deadlock: the waiting worker ran the second job inside the first"),
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("second job never ran"),
        ];
        finished.sort_unstable();
        assert_eq!(finished, vec![("first", 7), ("second", 0)]);
    }

    #[test]
    fn a_panicking_embed_job_is_an_error_not_a_hang() {
        let embedder = SharedEmbedder::load("fast", 2, 8).expect("fast preset");
        let result = embedder.run_on_embed_pool(|| -> u8 { panic!("boom") });
        assert!(result.is_err());
    }

    #[test]
    fn provider_setting_maps_onto_xberg_acceleration() {
        use xberg::core::config::ExecutionProviderType as Provider;
        assert!(
            acceleration(OnnxProvider::Auto).is_none(),
            "auto defers to xberg's platform default"
        );
        for (setting, expected) in [
            (OnnxProvider::Cpu, Provider::Cpu),
            (OnnxProvider::CoreMl, Provider::CoreMl),
            (OnnxProvider::Cuda, Provider::Cuda),
            (OnnxProvider::TensorRt, Provider::TensorRt),
        ] {
            assert_eq!(acceleration(setting).expect("pinned provider").provider, expected);
        }
        assert_eq!(
            OnnxProvider::default(),
            OnnxProvider::Cpu,
            "the bounded-memory provider is the default"
        );
    }

    #[test]
    fn bound_ort_memory_is_first_call_wins() {
        bound_ort_memory(2);
        // A second call with another value is a no-op (it must not clear live engine caches).
        bound_ort_memory(7);
    }

    #[test]
    fn resolve_embed_threads_nonzero_passthrough() {
        assert_eq!(resolve_embed_threads(4), 4);
        assert_eq!(resolve_embed_threads(1), 1);
        assert_eq!(resolve_embed_threads(16), 16);
    }

    #[test]
    fn resolve_embed_threads_zero_gives_auto() {
        let got = resolve_embed_threads(0);
        let expected = std::cmp::max(2, rayon::current_num_threads() / 4);
        assert_eq!(
            got, expected,
            "resolve_embed_threads(0) should yield max(2, cores/4) = {expected}"
        );
        assert!(got >= 2, "auto embed cap must be >= 2, got {got}");
    }

    #[test]
    fn a_load_failure_is_remembered_for_a_while_and_cleared_by_a_success() {
        let model = "load-failure-test-model";
        assert!(super::load_failure::recent(model).is_ok());
        let failed: Result<()> = Err(anyhow!("download of model weights failed: offline"));
        assert!(super::load_failure::remember(model, failed).is_err());
        let recent = super::load_failure::recent(model).unwrap_err().to_string();
        assert!(recent.contains("offline") && recent.contains("not retried"), "{recent}");
        assert!(super::load_failure::recent("another-model").is_ok());
        let one_input: Result<()> = Err(anyhow!("embed: input text must not be empty"));
        let _ = super::load_failure::remember("other", one_input);
        assert!(
            super::load_failure::recent("other").is_ok(),
            "one input failing is not a load failure"
        );
        assert!(super::load_failure::remember(model, Ok(())).is_ok());
        assert!(super::load_failure::recent(model).is_ok());
    }

    #[test]
    fn a_query_prefix_is_added_once() {
        assert_eq!(super::with_query_prefix(Some("query: "), "rivers"), "query: rivers");
        assert_eq!(
            super::with_query_prefix(Some("query: "), "query: rivers"),
            "query: rivers"
        );
        assert_eq!(super::with_query_prefix(None, "rivers"), "rivers");
    }
}

#[cfg(test)]
mod model_key_tests {
    use super::model_key;

    #[test]
    fn a_preset_is_identified_by_its_model_not_only_its_name() {
        let balanced = model_key("balanced");
        assert!(balanced.starts_with("balanced@"), "{balanced}");
        assert_eq!(balanced.len(), "balanced@".len() + 12);
        assert_eq!(balanced, model_key("balanced"), "stable within a build");
        assert_ne!(
            balanced.trim_start_matches("balanced@"),
            model_key("multilingual").trim_start_matches("multilingual@"),
            "two models of one dimension have different digests"
        );
        assert_eq!(model_key("not-a-preset"), "not-a-preset");
    }
}
