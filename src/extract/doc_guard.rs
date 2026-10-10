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
//!    wall-clock budget, so even an unforeseen stall costs one budget, not the scan.

use std::sync::mpsc;
use std::time::Duration;

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
    /// The work thread panicked or could not be spawned.
    Failed(String),
}

/// Run `work` on a dedicated thread and wait at most `budget` for its result. On timeout the thread is
/// abandoned (it cannot be cancelled mid-CPU-loop) and keeps running detached until it finishes on its
/// own; the caller is free immediately.
pub fn run_with_deadline<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, DeadlineError> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("basemind-doc-extract".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            // The receiver may be gone after a timeout; a late result is simply dropped.
            let _ = tx.send(work());
        })
        .map_err(|error| DeadlineError::Failed(format!("spawn extraction thread: {error}")))?;
    match rx.recv_timeout(budget) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(DeadlineError::Elapsed),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(DeadlineError::Failed("extraction thread panicked".to_string()))
        }
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

    #[test]
    fn deadline_returns_fast_work_and_abandons_slow_work() {
        assert_eq!(run_with_deadline(Duration::from_secs(5), || 7).unwrap(), 7);
        let started = std::time::Instant::now();
        let slow = run_with_deadline(Duration::from_millis(50), || std::thread::sleep(Duration::from_secs(3)));
        assert!(matches!(slow, Err(DeadlineError::Elapsed)));
        assert!(started.elapsed() < Duration::from_secs(2));
        let panicked: Result<(), _> = run_with_deadline(Duration::from_secs(5), || panic!("boom"));
        assert!(matches!(panicked, Err(DeadlineError::Failed(_))));
    }
}
