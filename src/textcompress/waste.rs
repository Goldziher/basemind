//! Waste detectors (token-reduction workstream W6).
//!
//! Read a JSON-Lines log of tool invocations and flag wasteful token
//! expenditure — redundant reads, repeated queries, and oversized reads — so a
//! `PostToolUse` hook (or the agent itself) can notice an anti-pattern. This is
//! pure analysis: it never executes anything and holds no global state.
//!
//! Two hard rules shape the design, mirroring the sibling `checkpoint` module:
//!
//! 1. **Findings are surfaced and persisted, so they must never carry a
//!    credential.** A `target` can be a query that embeds a secret; any finding
//!    whose `target` matches [`safety::contains_credential`] is dropped entirely
//!    before it can be emitted.
//! 2. **Determinism.** Findings are grouped via [`ahash::AHashMap`] then sorted
//!    by `(kind, target)`, so identical input always yields byte-identical
//!    output regardless of map iteration order.

use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use super::safety;

/// Minimum number of reads of the same path before a `redundant_read` finding
/// fires. The first read is necessary work; only the repeats are waste.
const READ_REPEAT_THRESHOLD: u32 = 2;
/// Minimum number of identical search/grep queries before a `repeated_query`
/// finding fires.
const QUERY_REPEAT_THRESHOLD: u32 = 2;
/// A single `Read` at or above this byte size is flagged `oversized_read` — the
/// suggestion being to use `outline` / `search_symbols` instead of a full read.
const LARGE_READ_BYTES: u64 = 32 * 1024;
/// Maximum number of findings retained. Past this the vec is truncated and
/// [`WasteReport::truncated`] is set, while `total_estimated_waste_bytes` still
/// counts every finding found before truncation (the headline number stays
/// honest even when the list is capped).
const MAX_FINDINGS: usize = 200;

/// The set of tool names treated as search/grep queries for the
/// `repeated_query` detector. `target` is the query string for these.
const QUERY_TOOLS: &[&str] = &[
    "Grep",
    "workspace_grep",
    "search_symbols",
    "find_references",
    "grep",
];

/// A single tool invocation parsed from one JSON-Lines record.
///
/// `target` is the file path (for reads) or query string (for searches);
/// `bytes` is the response size. Both are optional in the log and default so a
/// sparse record still parses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolCall {
    /// Tool name, e.g. `"Read"`, `"Grep"`, `"search_symbols"`.
    pub tool: String,
    /// File path (reads) or query string (searches). Defaults to empty.
    #[serde(default)]
    pub target: String,
    /// Response size in bytes. Defaults to `0`.
    #[serde(default)]
    pub bytes: u64,
}

/// One flagged anti-pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WasteFinding {
    /// Stable detector tag: `redundant_read`, `repeated_query`, or
    /// `oversized_read`.
    pub kind: String,
    /// The path or query the finding concerns.
    pub target: String,
    /// Number of contributing tool calls.
    pub count: u32,
    /// Estimated wasted bytes attributable to this finding.
    pub estimated_waste_bytes: u64,
}

/// The full analysis result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WasteReport {
    /// Deterministically ordered (`kind`, then `target`) findings, capped at
    /// [`MAX_FINDINGS`].
    pub findings: Vec<WasteFinding>,
    /// Sum of `estimated_waste_bytes` over **all** findings found, computed
    /// before any truncation so the headline stays honest when the list is
    /// capped.
    pub total_estimated_waste_bytes: u64,
    /// `true` when more than [`MAX_FINDINGS`] findings were found and the vec
    /// was truncated.
    pub truncated: bool,
}

/// Per-target accumulator for the repeat detectors: total occurrences and the
/// running sum of bytes after the first occurrence (the waste).
#[derive(Default)]
struct RepeatAccumulator {
    count: u32,
    waste_after_first: u64,
}

impl RepeatAccumulator {
    /// Record one more occurrence. The first occurrence is necessary work; every
    /// occurrence after the first adds its bytes to the waste total.
    fn observe(&mut self, bytes: u64) {
        if self.count >= 1 {
            self.waste_after_first = self.waste_after_first.saturating_add(bytes);
        }
        self.count = self.count.saturating_add(1);
    }
}

/// Analyse `calls` and return a deterministic, credential-safe [`WasteReport`].
///
/// Detectors:
/// 1. **Redundant reads** — `Read` calls grouped by `target`; when a path is
///    read `>= READ_REPEAT_THRESHOLD` times, one finding sums the bytes of every
///    read after the first.
/// 2. **Repeated queries** — search/grep calls ([`QUERY_TOOLS`]) grouped by
///    `target`; when a query appears `>= QUERY_REPEAT_THRESHOLD` times, one
///    finding sums the bytes after the first.
/// 3. **Oversized reads** — any single `Read` whose `bytes >= LARGE_READ_BYTES`
///    emits a finding (an oversized read may also belong to a redundant-read
///    group; the two findings are distinct).
///
/// Each finding's `target` is run through [`safety::contains_credential`] and
/// dropped if it matches. Findings are sorted by `(kind, target)`, then capped
/// at [`MAX_FINDINGS`] (`total_estimated_waste_bytes` is summed before the cap).
pub fn detect_waste(calls: &[ToolCall]) -> WasteReport {
    let mut reads: AHashMap<&str, RepeatAccumulator> = AHashMap::new();
    let mut queries: AHashMap<&str, RepeatAccumulator> = AHashMap::new();
    let mut findings: Vec<WasteFinding> = Vec::new();

    for call in calls {
        let target = call.target.as_str();
        if call.tool == "Read" {
            reads.entry(target).or_default().observe(call.bytes);
            // Oversized-read detector: independent of the redundant-read group.
            if call.bytes >= LARGE_READ_BYTES {
                findings.push(WasteFinding {
                    kind: "oversized_read".to_string(),
                    target: target.to_string(),
                    count: 1,
                    estimated_waste_bytes: call.bytes,
                });
            }
        } else if QUERY_TOOLS.contains(&call.tool.as_str()) {
            queries.entry(target).or_default().observe(call.bytes);
        }
    }

    for (target, acc) in reads {
        if acc.count >= READ_REPEAT_THRESHOLD {
            findings.push(WasteFinding {
                kind: "redundant_read".to_string(),
                target: target.to_string(),
                count: acc.count,
                estimated_waste_bytes: acc.waste_after_first,
            });
        }
    }

    for (target, acc) in queries {
        if acc.count >= QUERY_REPEAT_THRESHOLD {
            findings.push(WasteFinding {
                kind: "repeated_query".to_string(),
                target: target.to_string(),
                count: acc.count,
                estimated_waste_bytes: acc.waste_after_first,
            });
        }
    }

    // Hard security gate: a finding whose target embeds a credential is dropped
    // entirely (findings are surfaced and persisted).
    findings.retain(|f| !safety::contains_credential(&f.target));

    // Deterministic order, independent of map iteration order.
    findings.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.target.cmp(&b.target)));

    // Headline waste counts every finding BEFORE truncation.
    let total_estimated_waste_bytes = findings
        .iter()
        .fold(0u64, |sum, f| sum.saturating_add(f.estimated_waste_bytes));

    let truncated = findings.len() > MAX_FINDINGS;
    if truncated {
        findings.truncate(MAX_FINDINGS);
    }

    WasteReport {
        findings,
        total_estimated_waste_bytes,
        truncated,
    }
}

/// Parse a JSON-Lines blob into [`ToolCall`]s, leniently. A line that is not
/// valid JSON, or that parses but lacks a `tool` field, is silently skipped —
/// a malformed log must never abort or panic (fail-open). Blank lines are
/// ignored.
pub fn parse_calls(input: &str) -> Vec<ToolCall> {
    input
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<ToolCall>(line).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(target: &str, bytes: u64) -> ToolCall {
        ToolCall {
            tool: "Read".to_string(),
            target: target.to_string(),
            bytes,
        }
    }

    fn query(tool: &str, target: &str, bytes: u64) -> ToolCall {
        ToolCall {
            tool: tool.to_string(),
            target: target.to_string(),
            bytes,
        }
    }

    #[test]
    fn redundant_read_fires_at_two_reads_summing_bytes_after_first() {
        let calls = vec![
            read("src/main.rs", 100),
            read("src/main.rs", 100),
            read("src/main.rs", 100),
        ];
        let report = detect_waste(&calls);
        assert_eq!(report.findings.len(), 1);
        let f = &report.findings[0];
        assert_eq!(f.kind, "redundant_read");
        assert_eq!(f.target, "src/main.rs");
        assert_eq!(f.count, 3);
        assert_eq!(f.estimated_waste_bytes, 200);
        assert_eq!(report.total_estimated_waste_bytes, 200);
        assert!(!report.truncated);
    }

    #[test]
    fn single_read_yields_no_redundant_finding() {
        let report = detect_waste(&[read("src/lib.rs", 4096)]);
        assert_eq!(report.findings, Vec::<WasteFinding>::new());
        assert_eq!(report.total_estimated_waste_bytes, 0);
    }

    #[test]
    fn repeated_query_fires_for_two_identical_workspace_grep_targets() {
        let calls = vec![
            query("workspace_grep", "fn detect_waste", 50),
            query("workspace_grep", "fn detect_waste", 70),
        ];
        let report = detect_waste(&calls);
        assert_eq!(report.findings.len(), 1);
        let f = &report.findings[0];
        assert_eq!(f.kind, "repeated_query");
        assert_eq!(f.target, "fn detect_waste");
        assert_eq!(f.count, 2);
        assert_eq!(f.estimated_waste_bytes, 70);
    }

    #[test]
    fn oversized_read_fires_at_threshold_not_below() {
        let at = detect_waste(&[read("big.rs", LARGE_READ_BYTES)]);
        assert_eq!(at.findings.len(), 1);
        assert_eq!(at.findings[0].kind, "oversized_read");
        assert_eq!(at.findings[0].count, 1);
        assert_eq!(at.findings[0].estimated_waste_bytes, LARGE_READ_BYTES);

        let below = detect_waste(&[read("small.rs", LARGE_READ_BYTES - 1)]);
        assert_eq!(below.findings, Vec::<WasteFinding>::new());
    }

    #[test]
    fn oversized_read_coexists_with_redundant_read() {
        // Two oversized reads of the same path: one redundant_read finding plus
        // two oversized_read findings (distinct detectors).
        let calls = vec![
            read("huge.rs", LARGE_READ_BYTES),
            read("huge.rs", LARGE_READ_BYTES),
        ];
        let report = detect_waste(&calls);
        let kinds: Vec<&str> = report.findings.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["oversized_read", "oversized_read", "redundant_read"]
        );
    }

    #[test]
    fn drops_finding_whose_target_carries_github_pat() {
        let secret = format!("password=ghp_{}", "a".repeat(36));
        let calls = vec![query("Grep", &secret, 10), query("Grep", &secret, 10)];
        let report = detect_waste(&calls);
        assert!(
            report.findings.is_empty(),
            "credential-bearing finding must be dropped, got: {:?}",
            report.findings
        );
        assert_eq!(report.total_estimated_waste_bytes, 0);
    }

    #[test]
    fn drops_finding_whose_target_carries_aws_key() {
        let secret = "search AKIAIOSFODNN7EXAMPLE here";
        let calls = vec![
            query("search_symbols", secret, 10),
            query("search_symbols", secret, 10),
        ];
        let report = detect_waste(&calls);
        assert!(
            report.findings.is_empty(),
            "AWS-key finding must be dropped, got: {:?}",
            report.findings
        );
    }

    #[test]
    fn findings_are_deterministically_ordered() {
        // Mixed kinds and targets arriving in an order that would not match the
        // sorted output, exercised across detectors so map iteration order can
        // vary. The result vec must be sorted by (kind, target).
        let calls = vec![
            read("zzz.rs", LARGE_READ_BYTES),
            read("aaa.rs", 5),
            read("aaa.rs", 5),
            query("Grep", "qqq", 1),
            query("Grep", "qqq", 1),
            query("Grep", "aaa", 1),
            query("Grep", "aaa", 1),
            read("mmm.rs", 5),
            read("mmm.rs", 5),
        ];
        let report = detect_waste(&calls);
        let shape: Vec<(&str, &str)> = report
            .findings
            .iter()
            .map(|f| (f.kind.as_str(), f.target.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("oversized_read", "zzz.rs"),
                ("redundant_read", "aaa.rs"),
                ("redundant_read", "mmm.rs"),
                ("repeated_query", "aaa"),
                ("repeated_query", "qqq"),
            ]
        );
    }

    #[test]
    fn max_findings_cap_sets_truncated_and_keeps_full_waste_total() {
        // Build more than MAX_FINDINGS distinct oversized reads (each 1 byte over
        // a value chosen so the per-finding waste is known and the headline total
        // exceeds what survives the cap).
        let total = MAX_FINDINGS + 50;
        let calls: Vec<ToolCall> = (0..total)
            .map(|n| read(&format!("file_{n:05}.rs"), LARGE_READ_BYTES))
            .collect();
        let report = detect_waste(&calls);
        assert_eq!(report.findings.len(), MAX_FINDINGS);
        assert!(report.truncated);
        // The headline counts ALL findings before truncation.
        let expected_total = (total as u64) * LARGE_READ_BYTES;
        assert_eq!(report.total_estimated_waste_bytes, expected_total);
        // And it is strictly larger than the sum of the surviving (capped) vec.
        let surviving: u64 = report
            .findings
            .iter()
            .map(|f| f.estimated_waste_bytes)
            .sum();
        assert!(report.total_estimated_waste_bytes > surviving);
    }

    #[test]
    fn empty_input_yields_empty_report() {
        let report = detect_waste(&[]);
        assert_eq!(report.findings, Vec::<WasteFinding>::new());
        assert_eq!(report.total_estimated_waste_bytes, 0);
        assert!(!report.truncated);
    }

    #[test]
    fn parse_calls_skips_malformed_and_tool_less_lines() {
        let input = concat!(
            "{\"tool\":\"Read\",\"target\":\"a.rs\",\"bytes\":10}\n",
            "not json at all\n",
            "{\"target\":\"b.rs\",\"bytes\":20}\n", // missing `tool`
            "\n",
            "   \n",
            "{\"tool\":\"Grep\",\"target\":\"q\"}\n", // bytes defaults to 0
        );
        let calls = parse_calls(input);
        assert_eq!(
            calls,
            vec![
                ToolCall {
                    tool: "Read".to_string(),
                    target: "a.rs".to_string(),
                    bytes: 10,
                },
                ToolCall {
                    tool: "Grep".to_string(),
                    target: "q".to_string(),
                    bytes: 0,
                },
            ]
        );
    }
}
