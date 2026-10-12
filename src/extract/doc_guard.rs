//! Guards that keep one pathological document from stalling a scan.
//!
//! xberg's markdown splitter re-collects and re-sorts every remaining structural element for each
//! chunk it emits, so chunking costs O(chunks x elements) — quadratic in the document size. A
//! multi-megabyte csv, log or yaml file therefore pins a scan worker for minutes to hours, and because
//! the cost is CPU inside a synchronous call nothing inside xberg can interrupt it. Two defences:
//!
//! 1. [`prefers_linear_chunking`] routes large text-like files to [`linear_chunk_spans`], an O(n)
//!    fixed-size chunker, before the splitter ever runs.
//! 2. [`run_with_deadline`] runs the whole extraction on its own thread and gives up on it after the
//!    wall-clock budget, so even an unforeseen stall costs one budget, not the scan. Abandoned threads
//!    are counted and capped ([`AbandonedThreads`]), so stalls cannot accumulate into a pile of
//!    burning cores.
//! 3. [`DocPhase`] bounds the whole document phase of a scan, so several documents timing out
//!    together cost one total budget rather than one per-file budget each.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

/// Size cutovers above which a text-like document skips the markdown splitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkCutovers {
    /// Structured or marked-up text: markdown, csv, json, yaml, xml, toml, ini.
    pub markdown_bytes: u64,
    /// `text/plain`: `.txt`, `.log` and friends.
    pub plain_text_bytes: u64,
}

/// True for formats whose extracted text is roughly the size of the file. Binary containers (PDF,
/// Office, images) are excluded: their file size says nothing about the text they yield, and a
/// degraded chunker would throw away page structure they rely on.
fn is_text_like(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    mime.starts_with("text/")
        || ["json", "yaml", "xml", "toml", "csv"]
            .iter()
            .any(|marker| mime.contains(marker))
}

/// Whether a document of `file_len` bytes and type `mime` must bypass xberg's markdown splitter.
pub fn prefers_linear_chunking(mime: &str, file_len: u64, cutovers: ChunkCutovers) -> bool {
    if !is_text_like(mime) {
        return false;
    }
    let plain = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/plain");
    let limit = if plain {
        cutovers.plain_text_bytes
    } else {
        cutovers.markdown_bytes
    };
    file_len > limit
}

/// Fixed-size chunk spans (byte ranges into `text`) of at most `max_chars` characters, overlapping by
/// about `overlap` characters, in O(n). Each chunk prefers to end at a line break, then at whitespace,
/// in the back half of its window, and is hard-split on a character boundary only when a line is
/// longer than the window. Whitespace-only spans are dropped.
pub fn linear_chunk_spans(text: &str, max_chars: usize, overlap: usize) -> Vec<(usize, usize)> {
    let max_chars = max_chars.max(1);
    let overlap = overlap.min(max_chars.saturating_sub(1));
    let mut spans = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let rest = &text[start..];
        let window_end = rest
            .char_indices()
            .nth(max_chars)
            .map_or(text.len(), |(i, _)| start + i);
        let mut end = window_end;
        if window_end < text.len() {
            let window = &text[start..window_end];
            let half = window.len() / 2;
            let break_at = window.rfind('\n').map(|i| i + 1).filter(|&i| i > half).or_else(|| {
                window
                    .char_indices()
                    .rev()
                    .find(|(i, c)| c.is_whitespace() && i + c.len_utf8() > half)
                    .map(|(i, c)| i + c.len_utf8())
            });
            if let Some(offset) = break_at {
                end = start + offset;
            }
        }
        if !text[start..end].trim().is_empty() {
            spans.push((start, end));
        }
        if end >= text.len() {
            break;
        }
        let next = text[start..end]
            .char_indices()
            .rev()
            .nth(overlap.saturating_sub(1))
            .map_or(end, |(i, _)| start + i);
        start = if overlap == 0 || next <= start { end } else { next };
    }
    spans
}

/// Why [`run_with_deadline`] returned no value.
#[derive(Debug)]
pub enum DeadlineError {
    /// The work did not finish within the budget; its thread was abandoned.
    Elapsed,
    /// Too many earlier extractions were abandoned and are still burning CPU; nothing was spawned.
    AtCapacity { live: usize, cap: usize },
    /// The work thread panicked or could not be spawned.
    Failed(String),
}

/// Live count of abandoned extraction threads: timed out, detached, and not yet finished.
///
/// A thread that overran its budget cannot be cancelled mid-CPU-loop, so it keeps a core busy until it
/// finishes on its own. Counting those threads lets [`run_with_deadline`] refuse to start more of them
/// (the cap) and lets the scan report how many are still running.
#[derive(Debug, Clone, Default)]
pub struct AbandonedThreads {
    live: Arc<AtomicUsize>,
}

impl AbandonedThreads {
    /// The process-wide ledger every document extraction reports into.
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<AbandonedThreads> = OnceLock::new();
        GLOBAL.get_or_init(Self::default)
    }

    /// Abandoned threads that have not finished yet.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }
}

/// Abandoned extraction threads still running in this process (see [`AbandonedThreads`]).
pub fn abandoned_extractions() -> usize {
    AbandonedThreads::global().live()
}

const RUNNING: u8 = 0;
const ABANDONED: u8 = 1;
const FINISHED: u8 = 2;

/// Shared between the waiting caller and its work thread; decides, exactly once, who owns the
/// "abandoned" accounting so the live count can neither leak nor underflow.
struct ThreadSlot {
    state: AtomicU8,
    live: Arc<AtomicUsize>,
}

impl ThreadSlot {
    /// Caller side, on timeout. False when the thread finished in the meantime.
    fn abandon(&self) -> bool {
        // Count first: the thread decrements only after it observes ABANDONED, so the add must already
        // be visible or the counter could momentarily underflow.
        self.live.fetch_add(1, Ordering::AcqRel);
        let won = self
            .state
            .compare_exchange(RUNNING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if !won {
            self.live.fetch_sub(1, Ordering::AcqRel);
        }
        won
    }

    /// Thread side, however the work ended (value, panic).
    fn finish(&self) {
        if self.state.swap(FINISHED, Ordering::AcqRel) == ABANDONED {
            self.live.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Runs [`ThreadSlot::finish`] on drop, so a panicking work thread is still removed from the count.
struct FinishGuard(Arc<ThreadSlot>);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// Run `work` on a dedicated thread and wait at most `budget` for its result. On timeout the thread is
/// abandoned (it cannot be cancelled mid-CPU-loop) and keeps running detached until it finishes on its
/// own; the caller is free immediately.
///
/// Abandoned threads are tracked in `ledger`. When `cap` of them are already running no new thread is
/// spawned and [`DeadlineError::AtCapacity`] is returned at once, so a corpus full of pathological files
/// cannot pile up one burning core per timeout. The cap is a soft bound: extractions already in flight
/// when it is reached may still time out, so the live count can exceed it by up to the worker count.
///
/// # Why a thread and not a killable subprocess
///
/// A child process would be killable, and was evaluated. It is not worth it here: extraction runs
/// against in-process state (the shared Tokio runtime, the ONNX embedder and xberg's loaded models), so
/// a child must reload those models per document or be a long-lived worker pool with its own protocol;
/// the extracted document, chunks and embedding vectors would cross a pipe as a serialised blob; the
/// memory-footprint gate would have to account for children it cannot see; and it needs a self-exec
/// path, a kill/reap story and a Windows variant. The cap bounds the damage a stuck thread does at the
/// cost of deferring documents, with none of that machinery.
pub fn run_with_deadline<T: Send + 'static>(
    ledger: &AbandonedThreads,
    cap: usize,
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, DeadlineError> {
    let live = ledger.live();
    if live >= cap {
        return Err(DeadlineError::AtCapacity { live, cap });
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let slot = Arc::new(ThreadSlot {
        state: AtomicU8::new(RUNNING),
        live: Arc::clone(&ledger.live),
    });
    let guard = FinishGuard(Arc::clone(&slot));
    std::thread::Builder::new()
        .name("basemind-doc-extract".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let _guard = guard;
            // The receiver may be gone after a timeout; a late result is simply dropped.
            let _ = tx.send(work());
        })
        .map_err(|error| DeadlineError::Failed(format!("spawn extraction thread: {error}")))?;
    match rx.recv_timeout(budget) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if slot.abandon() {
                Err(DeadlineError::Elapsed)
            } else {
                // Finished between the timeout and the abandon: the result (or the panic) is final.
                rx.try_recv()
                    .map_err(|_| DeadlineError::Failed("extraction thread panicked".to_string()))
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(DeadlineError::Failed("extraction thread panicked".to_string()))
        }
    }
}

/// Wall-clock budget for the whole document phase of one scan (`[documents] phase_budget_secs`).
///
/// The clock starts at the first document that needs a real extraction (a cached blob never asks), so
/// code files processed before the first document do not eat into it. Once spent, [`DocPhase::admit`]
/// refuses every later document; the caller defers them to the next scan. Documents already running
/// finish or hit their own per-file budget, so the phase can overshoot by up to one per-file budget.
#[derive(Debug, Default)]
pub struct DocPhase {
    state: Mutex<PhaseState>,
}

#[derive(Debug, Default)]
struct PhaseState {
    budget: Option<Duration>,
    started: Option<Instant>,
}

/// The document-phase budget is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseSpent {
    pub elapsed: Duration,
    pub budget: Duration,
}

impl DocPhase {
    /// A phase with `budget` that has not started its clock yet.
    pub fn with_budget(budget: Duration) -> Self {
        Self {
            state: Mutex::new(PhaseState {
                budget: Some(budget),
                started: None,
            }),
        }
    }

    /// Re-arm for a new scan: new `budget`, clock not started. Filters outlive a single scan in the
    /// watcher, so every scan begins by resetting its phase.
    pub fn begin(&self, budget: Duration) {
        *self.lock() = PhaseState {
            budget: Some(budget),
            started: None,
        };
    }

    /// Admit one more document, starting the clock on the first call.
    pub fn admit(&self) -> Result<(), PhaseSpent> {
        self.admit_at(Instant::now())
    }

    fn admit_at(&self, now: Instant) -> Result<(), PhaseSpent> {
        let mut state = self.lock();
        let Some(budget) = state.budget else {
            return Ok(());
        };
        let started = *state.started.get_or_insert(now);
        let elapsed = now.saturating_duration_since(started);
        if elapsed >= budget {
            Err(PhaseSpent { elapsed, budget })
        } else {
            Ok(())
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PhaseState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUTOVERS: ChunkCutovers = ChunkCutovers {
        markdown_bytes: 1000,
        plain_text_bytes: 100,
    };

    #[test]
    fn routes_by_mime_family_and_size() {
        assert!(prefers_linear_chunking("text/plain", 101, CUTOVERS));
        assert!(!prefers_linear_chunking("text/plain", 100, CUTOVERS));
        assert!(!prefers_linear_chunking("text/markdown", 1000, CUTOVERS));
        assert!(prefers_linear_chunking("text/csv", 1001, CUTOVERS));
        assert!(prefers_linear_chunking(
            "application/json; charset=utf-8",
            1001,
            CUTOVERS
        ));
        assert!(prefers_linear_chunking("application/x-yaml", 1001, CUTOVERS));
        assert!(!prefers_linear_chunking("application/pdf", u64::MAX, CUTOVERS));
        assert!(!prefers_linear_chunking("", u64::MAX, CUTOVERS));
    }

    fn assert_valid(text: &str, spans: &[(usize, usize)], max_chars: usize) {
        let mut prev_end = 0;
        for &(s, e) in spans {
            assert!(text.is_char_boundary(s) && text.is_char_boundary(e));
            assert!(s < e && e > prev_end, "spans advance");
            assert!(text[s..e].chars().count() <= max_chars);
            assert!(s <= prev_end, "no gap between spans");
            prev_end = e;
        }
    }

    #[test]
    fn linear_chunks_cover_text_without_gaps_and_prefer_line_breaks() {
        let text: String = (0..500).map(|i| format!("line number {i}\n")).collect();
        let spans = linear_chunk_spans(&text, 100, 20);
        assert_valid(&text, &spans, 100);
        assert_eq!(spans.last().unwrap().1, text.len());
        assert!(spans.iter().all(|&(_, e)| text.as_bytes()[e - 1] == b'\n'));
    }

    #[test]
    fn linear_chunks_hard_split_long_lines_on_char_boundaries() {
        let text = "é".repeat(1000);
        let spans = linear_chunk_spans(&text, 64, 8);
        assert_valid(&text, &spans, 64);
        assert_eq!(spans.last().unwrap().1, text.len());
    }

    #[test]
    fn linear_chunks_handle_degenerate_inputs() {
        assert!(linear_chunk_spans("", 64, 8).is_empty());
        assert!(linear_chunk_spans("   \n\n  ", 64, 8).is_empty());
        assert_eq!(linear_chunk_spans("short", 64, 8), vec![(0, 5)]);
        let spans = linear_chunk_spans(&"x".repeat(300), 64, 0);
        assert_eq!(spans.len(), 5);
    }

    const ROOMY: usize = 8;

    #[test]
    fn deadline_returns_fast_work_and_abandons_slow_work() {
        let ledger = AbandonedThreads::default();
        let run = |budget, work: fn() -> i32| run_with_deadline(&ledger, ROOMY, budget, work);
        assert_eq!(run(Duration::from_secs(5), || 7).unwrap(), 7);
        let started = Instant::now();
        let slow = run(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(600));
            0
        });
        assert!(matches!(slow, Err(DeadlineError::Elapsed)));
        assert!(started.elapsed() < Duration::from_millis(500));
        let panicked = run(Duration::from_secs(5), || panic!("boom"));
        assert!(matches!(panicked, Err(DeadlineError::Failed(_))));
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn abandoned_threads_are_counted_until_they_finish() {
        let ledger = AbandonedThreads::default();
        let (release, gate) = mpsc::channel::<()>();
        let slow = run_with_deadline(&ledger, ROOMY, Duration::from_millis(30), move || {
            let _ = gate.recv();
        });
        assert!(matches!(slow, Err(DeadlineError::Elapsed)));
        assert_eq!(ledger.live(), 1, "the overrunning thread is still running");
        release.send(()).unwrap();
        wait_until("the abandoned thread to finish", || ledger.live() == 0);
    }

    #[test]
    fn a_panicking_abandoned_thread_still_leaves_the_count() {
        let ledger = AbandonedThreads::default();
        let (release, gate) = mpsc::channel::<()>();
        let slow = run_with_deadline(&ledger, ROOMY, Duration::from_millis(30), move || {
            let _ = gate.recv();
            panic!("late boom");
        });
        assert!(matches!(slow, Err(DeadlineError::Elapsed)));
        assert_eq!(ledger.live(), 1);
        release.send(()).unwrap();
        wait_until("the panicked thread to leave the count", || ledger.live() == 0);
    }

    #[test]
    fn at_the_cap_no_further_thread_is_spawned() {
        let ledger = AbandonedThreads::default();
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        for _ in 0..2 {
            let gate = Arc::clone(&gate);
            let slow = run_with_deadline(&ledger, 2, Duration::from_millis(30), move || {
                let _ = gate.lock().unwrap().recv();
            });
            assert!(matches!(slow, Err(DeadlineError::Elapsed)));
        }
        assert_eq!(ledger.live(), 2);

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let refused = run_with_deadline(&ledger, 2, Duration::from_secs(5), move || {
            flag.store(true, Ordering::SeqCst);
        });
        assert!(matches!(refused, Err(DeadlineError::AtCapacity { live: 2, cap: 2 })));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!ran.load(Ordering::SeqCst), "a refused extraction must not run");

        drop(release);
        wait_until("abandoned threads to drain", || ledger.live() == 0);
        assert_eq!(
            run_with_deadline(&ledger, 2, Duration::from_secs(5), || 1).unwrap(),
            1,
            "capacity returns once the stuck threads finish"
        );
    }

    #[test]
    fn phase_budget_starts_on_first_document_and_refuses_after_it_is_spent() {
        let phase = DocPhase::with_budget(Duration::from_secs(10));
        let t0 = Instant::now();
        assert!(phase.admit_at(t0).is_ok(), "first document starts the clock");
        assert!(phase.admit_at(t0 + Duration::from_secs(9)).is_ok());
        let spent = phase.admit_at(t0 + Duration::from_secs(10)).unwrap_err();
        assert_eq!(spent.budget, Duration::from_secs(10));
        assert_eq!(spent.elapsed, Duration::from_secs(10));
        assert!(phase.admit_at(t0 + Duration::from_secs(500)).is_err(), "stays spent");

        phase.begin(Duration::from_secs(10));
        assert!(
            phase.admit_at(t0 + Duration::from_secs(500)).is_ok(),
            "begin re-arms the phase"
        );
    }

    #[test]
    fn an_unarmed_phase_admits_everything() {
        let phase = DocPhase::default();
        assert!(phase.admit_at(Instant::now()).is_ok());
    }
}
