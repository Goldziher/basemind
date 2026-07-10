//! Append-only per-tool-call telemetry. One JSONL row per successful MCP tool dispatch, written
//! to `.basemind/telemetry.jsonl`. Powers the live statusline (`plugins/basemind/statusline.sh`)
//! and the `telemetry_summary` MCP tool.
//!
//! Telemetry is best-effort. Disk-full / permission-denied / serialize-failed all log via
//! `tracing::warn!` and continue; we never break a tool response because the dashboard couldn't
//! be updated.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::savings::SavingsRow;

/// Filename relative to `.basemind/`. Single source of truth so the statusline
/// script and the `telemetry_summary` reader resolve to the same path.
pub const TELEMETRY_FILENAME: &str = "telemetry.jsonl";

/// A single tool-call row, serialized as one JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryRow {
    /// Wall-clock microseconds since the Unix epoch.
    pub ts_micros: i64,
    /// MCP tool name (e.g. `"outline"`).
    pub tool: String,
    /// 16-hex-char blake3 prefix of the canonicalised params JSON. Enough to
    /// dedupe repeat calls in the dashboard without leaking content.
    pub params_hash: String,
    /// Serialized response body byte count. Reused from the json_result path.
    pub resp_bytes: u64,
    /// Wall-clock milliseconds from dispatch to response.
    pub elapsed_ms: u64,
    /// Estimated tokens saved vs the disclosed baseline. See `super::savings`.
    pub est_tokens_saved: u64,
    /// Disclosed baseline name (e.g. `"full_file_read"`, `"no_baseline"`).
    pub saved_baseline: String,
}

/// The telemetry writer. `Telemetry::record` is cheap and lock-protected — concurrent in-flight
/// MCP tool calls serialize on the underlying file handle.
pub struct Telemetry {
    path: PathBuf,
    writer: Mutex<Option<BufWriter<File>>>,
}

impl Telemetry {
    /// Construct a telemetry handle. The file isn't opened until the first record — lets
    /// `basemind serve` boot without touching the filesystem if no one ever queries it.
    pub fn new(basemind_dir: &Path) -> Self {
        Self {
            path: basemind_dir.join(TELEMETRY_FILENAME),
            writer: Mutex::new(None),
        }
    }

    /// Path of the underlying JSONL file (whether or not it has been created yet).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one tool-call row. Errors are logged via `tracing::warn!` and swallowed — telemetry
    /// is best-effort and must not affect tool response semantics.
    pub fn record(&self, tool: &str, params: &Value, resp_bytes: u64, elapsed_ms: u64, savings: &SavingsRow) {
        let row = TelemetryRow {
            ts_micros: now_micros(),
            tool: tool.to_string(),
            params_hash: hash_params(params),
            resp_bytes,
            elapsed_ms,
            est_tokens_saved: savings.est_tokens_saved,
            saved_baseline: savings.baseline.to_string(),
        };
        if let Err(e) = self.write_row(&row) {
            tracing::warn!(error = %e, tool, "telemetry: write failed (continuing)");
        }
    }

    fn write_row(&self, row: &TelemetryRow) -> std::io::Result<()> {
        let line = serde_json::to_vec(row).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut guard = self.writer.lock().expect("telemetry mutex poisoned");
        if guard.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
            *guard = Some(BufWriter::new(file));
        }
        let w = guard.as_mut().expect("writer just initialized");
        w.write_all(&line)?;
        w.write_all(b"\n")?;
        w.flush()?;
        Ok(())
    }
}

/// Wall-clock microseconds since the Unix epoch. Falls back to 0 on the (essentially impossible)
/// SystemTime-before-epoch error so the telemetry path can't panic.
fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 16-hex-char blake3 prefix of a canonical JSON representation of `params`.
fn hash_params(params: &Value) -> String {
    let canonical = serde_json::to_vec(params).unwrap_or_default();
    let hash = blake3::hash(&canonical);
    let mut out = String::with_capacity(16);
    for b in &hash.as_bytes()[..8] {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Cap on how many JSONL rows the dashboard inspects per call. Bounds the cost on
/// long-lived servers; a `truncated: true` flag is returned when we hit it.
const TELEMETRY_SUMMARY_READ_CAP: usize = 10_000;
/// How many of the most recent calls to surface in the `recent` field.
const TELEMETRY_SUMMARY_RECENT_COUNT: usize = 10;

/// Read the JSONL telemetry log at `path`, filter by `params.window` + `params.tool`,
/// aggregate per-tool + per-baseline counts, and return the dashboard payload.
///
/// All blocking I/O is offloaded via `spawn_blocking` so the MCP server stays
/// responsive when the JSONL is large (read cap: 10 000 rows).
pub(super) async fn summarize(
    path: &std::path::Path,
    params: super::types::TelemetrySummaryParams,
) -> Result<super::types::TelemetrySummaryResponse, rmcp::ErrorData> {
    use rmcp::ErrorData as McpError;

    let window = params.window.as_deref().unwrap_or("today").to_string();
    let cutoff_micros = window_cutoff_micros(&window)
        .map_err(|e| McpError::invalid_params(format!("unknown window `{window}`: {e}"), None))?;
    let tool_filter = params.tool.clone();

    let path_buf = path.to_path_buf();
    let rows = tokio::task::spawn_blocking(move || read_telemetry_tail(&path_buf))
        .await
        .map_err(|e| McpError::internal_error(format!("telemetry read join: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("telemetry read: {e}"), None))?;
    let truncated = rows.len() >= TELEMETRY_SUMMARY_READ_CAP;

    let mut per_tool: ahash::AHashMap<String, (usize, u64)> = ahash::AHashMap::new();
    let mut per_baseline: ahash::AHashMap<String, (usize, u64)> = ahash::AHashMap::new();
    let mut total_calls: usize = 0;
    let mut total_resp_bytes: u64 = 0;
    let mut total_saved: u64 = 0;
    let mut recent: Vec<super::types::RecentCallView> = Vec::with_capacity(TELEMETRY_SUMMARY_RECENT_COUNT);

    for row in rows.iter().rev() {
        if let Some(c) = cutoff_micros
            && row.ts_micros < c
        {
            continue;
        }
        if let Some(ref f) = tool_filter
            && &row.tool != f
        {
            continue;
        }
        total_calls += 1;
        total_resp_bytes = total_resp_bytes.saturating_add(row.resp_bytes);
        total_saved = total_saved.saturating_add(row.est_tokens_saved);
        let e = per_tool.entry(row.tool.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 = e.1.saturating_add(row.est_tokens_saved);
        let b = per_baseline.entry(row.saved_baseline.clone()).or_insert((0, 0));
        b.0 += 1;
        b.1 = b.1.saturating_add(row.est_tokens_saved);
        if recent.len() < TELEMETRY_SUMMARY_RECENT_COUNT {
            recent.push(super::types::RecentCallView {
                ts_micros: row.ts_micros,
                tool: row.tool.clone(),
                resp_bytes: row.resp_bytes,
                elapsed_ms: row.elapsed_ms,
                est_tokens_saved: row.est_tokens_saved,
            });
        }
    }
    let mut per_tool_vec: Vec<super::types::ToolCallCount> = per_tool
        .into_iter()
        .map(|(tool, (calls, est))| super::types::ToolCallCount {
            tool,
            calls,
            est_tokens_saved: est,
        })
        .collect();
    per_tool_vec.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.tool.cmp(&b.tool)));
    let mut per_baseline_vec: Vec<super::types::BaselineCount> = per_baseline
        .into_iter()
        .map(|(baseline, (calls, est))| super::types::BaselineCount {
            baseline,
            calls,
            est_tokens_saved: est,
        })
        .collect();
    per_baseline_vec.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.baseline.cmp(&b.baseline)));

    Ok(super::types::TelemetrySummaryResponse {
        window,
        total_calls,
        total_resp_bytes,
        total_est_tokens_saved: total_saved,
        per_tool: per_tool_vec,
        per_baseline: per_baseline_vec,
        recent,
        truncated,
        savings_note: "Savings are estimates vs a grep+Read baseline; see /basemind-stats for the model.",
    })
}

/// Convert a window string (`"today"` / `"1h"` / `"all"`) to a unix-microseconds cutoff.
/// Returns `None` for `"all"` (no cutoff).
fn window_cutoff_micros(window: &str) -> Result<Option<i64>, &'static str> {
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    match window {
        "all" => Ok(None),
        "1h" => Ok(Some(now_us.saturating_sub(3_600 * 1_000_000))),
        "24h" => Ok(Some(now_us.saturating_sub(24 * 3_600 * 1_000_000))),
        "today" => Ok(Some(now_us.saturating_sub(24 * 3_600 * 1_000_000))),
        _ => Err("expected one of: today, 1h, 24h, all"),
    }
}

/// Read up to [`TELEMETRY_SUMMARY_READ_CAP`] rows from the JSONL tail, oldest-first.
/// Missing file = empty vec (no panic, no error — the dashboard just shows zeros).
///
/// Uses a `VecDeque` ring-buffer so that evicting the oldest row during a bounded read is
/// O(1) (`pop_front`) rather than O(n) (`Vec::remove(0)`), which matters on long-lived
/// servers with millions of telemetry rows.
fn read_telemetry_tail(path: &std::path::Path) -> Result<Vec<TelemetryRow>, std::io::Error> {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader};
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let reader = BufReader::new(file);
    let mut rows: VecDeque<TelemetryRow> = VecDeque::with_capacity(TELEMETRY_SUMMARY_READ_CAP);
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(row) = serde_json::from_str::<TelemetryRow>(&line) {
            if rows.len() == TELEMETRY_SUMMARY_READ_CAP {
                rows.pop_front();
            }
            rows.push_back(row);
        }
    }
    Ok(rows.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn row_count(path: &Path) -> usize {
        std::fs::read_to_string(path).map(|s| s.lines().count()).unwrap_or(0)
    }

    #[test]
    fn records_append_to_jsonl_file() {
        let dir = tempdir().unwrap();
        let tel = Telemetry::new(dir.path());
        let savings = SavingsRow {
            baseline_tokens: 500,
            actual_tokens: 100,
            est_tokens_saved: 400,
            baseline: "full_file_read",
        };
        tel.record("outline", &json!({ "path": "a.rs" }), 400, 4, &savings);
        tel.record("outline", &json!({ "path": "b.rs" }), 300, 3, &savings);
        let path = dir.path().join(TELEMETRY_FILENAME);
        assert_eq!(row_count(&path), 2);
        let raw = std::fs::read_to_string(&path).unwrap();
        let first: TelemetryRow = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(first.tool, "outline");
        assert_eq!(first.resp_bytes, 400);
        assert_eq!(first.est_tokens_saved, 400);
        assert_eq!(first.saved_baseline, "full_file_read");
        assert_eq!(first.params_hash.len(), 16);
    }

    #[test]
    fn params_hash_is_deterministic_per_input() {
        let a = hash_params(&json!({ "k": 1, "v": "x" }));
        let b = hash_params(&json!({ "k": 1, "v": "x" }));
        let c = hash_params(&json!({ "k": 1, "v": "y" }));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    /// `read_telemetry_tail` evicts the oldest row (not the newest) once the cap is hit,
    /// and returns rows in oldest-first order so that `summarize` can iterate `.rev()` for
    /// the most-recent-N window.
    #[test]
    fn tail_read_evicts_oldest_and_preserves_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(TELEMETRY_FILENAME);
        let savings = SavingsRow {
            baseline_tokens: 100,
            actual_tokens: 10,
            est_tokens_saved: 90,
            baseline: "test",
        };
        let tel = Telemetry::new(dir.path());
        for i in 0..(TELEMETRY_SUMMARY_READ_CAP + 2) {
            tel.record(&format!("tool_{i:05}"), &json!({}), 0, 0, &savings);
        }

        let rows = read_telemetry_tail(&path).unwrap();

        assert_eq!(rows.len(), TELEMETRY_SUMMARY_READ_CAP);

        assert_eq!(
            rows[0].tool, "tool_00002",
            "oldest two rows must be evicted; first survivor should be tool_00002"
        );
        let last_expected = format!("tool_{:05}", TELEMETRY_SUMMARY_READ_CAP + 1);
        assert_eq!(
            rows[rows.len() - 1].tool,
            last_expected,
            "last row must be the most-recently written one"
        );
        for w in rows.windows(2) {
            assert!(
                w[0].tool < w[1].tool,
                "rows must be in oldest-first (ascending) order: {} >= {}",
                w[0].tool,
                w[1].tool
            );
        }
    }
}
