//! Task-file model: one JSON object per line.
//!
//! ```json
//! {"id":"sym-1","mode":"symbols","args":{"name":"Widget"},"gold":["src/w.py:12"],
//!  "scoring":"set","k":10,"baseline":{"grep":"git grep -n Widget","read":["src/w.py"]}}
//! ```

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Retrieval modes (ADR-0022 "task kinds") the harness can drive. Each maps onto one MCP tool;
/// several share `code_refs`, `git_log`, `git_blame` or `git_diff`; the analysis kinds of 0.34
/// (`impact` .. `explain`, ADR-0047) drive the engine op of their command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalMode {
    Symbols,
    Outline,
    References,
    Callers,
    Grep,
    Find,
    Dependents,
    GitSearch,
    Docs,
    /// `code_refs` with `relation: "implementations"`: `path:line` of each trait impl.
    Implementations,
    /// `code_read` with `symbol`: every `path:line` of the symbol's span.
    Read,
    /// `git_log` with `path`: the commits touching the file.
    GitPath,
    /// `git_log` with `path` + `symbol`: the commits that changed the symbol's body.
    GitSymbol,
    /// `git_blame` over a line span: `sha:line` per line.
    Blame,
    /// `git_blame` with `symbol`: `sha:line` per line of the symbol.
    BlameSymbol,
    /// `git_diff` between two revisions: `+path:line` added, `-path:line` removed.
    Diff,
    /// `git_diff` with `structural`: `+name` symbols added, `-name` removed.
    DiffStructural,
    /// `code_refs` with `relation: "importers"`, `path` and `depth`: every file that reaches `path`
    /// within `depth` hops of the file-level import graph (#121).
    Impact,
    /// `code_refs` with `relation: "cycles"`: every file in an import cycle, or with `path` the files
    /// of the cycle holding it (#135).
    Cycles,
    /// `code_grep` with `changed`: `path:line` of each match in the files a change touched (#122).
    GrepChanged,
    /// `find_files` with `min_lines`: every file at least that many lines long (#127).
    FindLarge,
    /// `code_grep` with `ranges`, then `code_read` with those ranges per file: every `path:line`
    /// within the context of a match (#129).
    ReadAround,
    /// `code explain` on a symbol (CLI-only): its definition, its call sites and `->name` per callee
    /// (#120).
    Explain,
}

impl EvalMode {
    /// The telemetry key (the tool name) of the tool call this mode drives; also the key
    /// `savings::estimate_from_text` classifies on.
    pub fn tool_key(self) -> &'static str {
        match self {
            Self::Symbols => "code_symbols",
            Self::Outline => "code_outline",
            Self::References | Self::Callers | Self::Dependents => "code_refs",
            Self::Grep => "code_grep",
            Self::Find => "find_files",
            Self::GitSearch => "git_log",
            Self::Docs => "docs_search",
            Self::Implementations => "code_refs",
            Self::Read => "code_read",
            Self::GitPath | Self::GitSymbol => "git_log",
            Self::Blame | Self::BlameSymbol => "git_blame",
            Self::Diff | Self::DiffStructural => "git_diff",
            Self::Impact | Self::Cycles => "code_refs",
            Self::GrepChanged => "code_grep",
            Self::FindLarge => "find_files",
            Self::ReadAround => "code_read",
            Self::Explain => "code_explain",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symbols => "symbols",
            Self::Outline => "outline",
            Self::References => "references",
            Self::Callers => "callers",
            Self::Grep => "grep",
            Self::Find => "find",
            Self::Dependents => "dependents",
            Self::GitSearch => "git_search",
            Self::Docs => "docs",
            Self::Implementations => "implementations",
            Self::Read => "read",
            Self::GitPath => "git_path",
            Self::GitSymbol => "git_symbol",
            Self::Blame => "blame",
            Self::BlameSymbol => "blame_symbol",
            Self::Diff => "diff",
            Self::DiffStructural => "diff_structural",
            Self::Impact => "impact",
            Self::Cycles => "cycles",
            Self::GrepChanged => "grep_changed",
            Self::FindLarge => "find_large",
            Self::ReadAround => "read_around",
            Self::Explain => "explain",
        }
    }
}

/// How a task is scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scoring {
    /// Order-insensitive: precision / recall / F1 over the whole answer.
    #[default]
    Set,
    /// Order matters: P/R/F1 at `k` plus hit@n, MRR and nDCG@k.
    Ranked,
}

/// A single string or a list of strings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn items(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// How the baseline's output is turned into items scored against the same gold as the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineItems {
    /// `path:line:text` lines (`git grep -n`, `rg -n`): `path:line`.
    GrepLines,
    /// One path per line (`git ls-files | grep`, `git grep -l`): `path`.
    Paths,
    /// A commit sha (full or abbreviated) first on each line (`git log --oneline`): the full sha.
    Shas,
    /// `git blame` default output: `sha:line` per blamed line.
    Blame,
    /// A unified diff: `+path:line` per added line, `-path:line` per removed line.
    Diff,
    /// A unified diff: `+name` / `-name` per definition whose name appears only on added / removed
    /// lines.
    DiffSymbols,
    /// Every line of every file in `read`: the agent holds the whole file.
    Read,
    /// `wc -l`-style `<count> <path>` lines: `path`. A `total` row is not a file.
    CountedPaths,
    /// `rg -n -C` / `grep -n -C` context output: `path:line:text` for a match, `path-line-text` for
    /// a context line, `--` between groups: `path:line` for both.
    GrepContext,
}

/// What an agent would do without basemind: shell command(s) run in the workspace root plus whole
/// files read. The output of all of it is what the token comparison charges.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BaselineSpec {
    /// Shell command(s) (`sh -c`), e.g. `git grep -n -w Widget -- '*.py'`.
    #[serde(default)]
    pub grep: Option<OneOrMany>,
    /// Repo-relative files read in full.
    #[serde(default)]
    pub read: Vec<String>,
    /// How to read the output as items, so the baseline is scored against the gold too. Absent:
    /// the baseline is charged for tokens only.
    #[serde(default)]
    pub items: Option<BaselineItems>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Task {
    pub id: String,
    pub mode: EvalMode,
    /// Tool arguments for the mode (`name`, `pattern`, `path`, `query`, ...), exactly as the MCP
    /// tool takes them.
    #[serde(default)]
    pub args: Value,
    /// Expected items: `path` or `path:line`.
    #[serde(default)]
    pub gold: Vec<String>,
    #[serde(default)]
    pub scoring: Scoring,
    /// Cut-off for ranked scoring (defaults to 10).
    #[serde(default)]
    pub k: Option<usize>,
    /// Allowed distance between a returned and a gold line (defaults to 0).
    #[serde(default)]
    pub line_slack: u32,
    #[serde(default)]
    pub baseline: Option<BaselineSpec>,
    /// `grep` only: `path:line` -> the name of the definition the line sits in. Scored as the
    /// share of returned gold hits whose attached symbol carries that name.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

impl Task {
    pub fn k(&self) -> usize {
        self.k.unwrap_or(10).max(1)
    }
}

/// Parse a JSONL task file. Blank lines and `#` comment lines are skipped; ids must be unique.
pub fn parse_tasks(text: &str) -> Result<Vec<Task>> {
    let mut tasks: Vec<Task> = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let task: Task = serde_json::from_str(line).with_context(|| format!("task file line {}", idx + 1))?;
        if !(task.args.is_object() || task.args.is_null()) {
            bail!("task `{}` (line {}): `args` must be an object", task.id, idx + 1);
        }
        if tasks.iter().any(|t| t.id == task.id) {
            bail!("duplicate task id `{}` (line {})", task.id, idx + 1);
        }
        tasks.push(task);
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_task_and_defaults() {
        let t = parse_tasks(
            "# c\n{\"id\":\"a\",\"mode\":\"git_search\",\"args\":{\"query\":\"x\"},\"gold\":[\"f.py:3\"],\
             \"scoring\":\"ranked\",\"k\":3,\"baseline\":{\"grep\":\"git grep x\",\"read\":[\"f.py\"]}}\n\n\
             {\"id\":\"b\",\"mode\":\"docs\"}\n",
        )
        .unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].mode, EvalMode::GitSearch);
        assert_eq!(t[0].scoring, Scoring::Ranked);
        assert_eq!(t[0].k(), 3);
        assert_eq!(t[0].baseline.as_ref().unwrap().read, vec!["f.py"]);
        assert_eq!(t[1].scoring, Scoring::Set);
        assert_eq!(t[1].k(), 10);
        assert!(t[1].gold.is_empty());
    }

    #[test]
    fn rejects_duplicates_unknown_modes_and_bad_args() {
        assert!(parse_tasks("{\"id\":\"a\",\"mode\":\"grep\"}\n{\"id\":\"a\",\"mode\":\"grep\"}").is_err());
        assert!(parse_tasks("{\"id\":\"a\",\"mode\":\"nope\"}").is_err());
        assert!(parse_tasks("{\"id\":\"a\",\"mode\":\"grep\",\"args\":[1]}").is_err());
    }

    #[test]
    fn parses_new_kinds_and_baseline_item_readers() {
        let t = parse_tasks(
            "{\"id\":\"a\",\"mode\":\"blame_symbol\",\"baseline\":{\"grep\":\"git blame f\",\"items\":\"blame\"}}\n\
             {\"id\":\"b\",\"mode\":\"diff_structural\"}",
        )
        .unwrap();
        assert_eq!(t[0].mode, EvalMode::BlameSymbol);
        assert_eq!(t[0].mode.tool_key(), "git_blame");
        assert_eq!(t[0].baseline.as_ref().unwrap().items, Some(BaselineItems::Blame));
        assert_eq!(t[1].mode.as_str(), "diff_structural");
        assert!(parse_tasks("{\"id\":\"a\",\"mode\":\"grep\",\"baseline\":{\"items\":\"nope\"}}").is_err());
    }

    #[test]
    fn parses_the_analysis_kinds_with_their_tools() {
        let kinds = [
            ("impact", EvalMode::Impact, "code_refs"),
            ("cycles", EvalMode::Cycles, "code_refs"),
            ("grep_changed", EvalMode::GrepChanged, "code_grep"),
            ("find_large", EvalMode::FindLarge, "find_files"),
            ("read_around", EvalMode::ReadAround, "code_read"),
            ("explain", EvalMode::Explain, "code_explain"),
        ];
        for (name, mode, tool) in kinds {
            let t = parse_tasks(&format!("{{\"id\":\"a\",\"mode\":\"{name}\"}}")).unwrap();
            assert_eq!(t[0].mode, mode);
            assert_eq!((mode.as_str(), mode.tool_key()), (name, tool));
        }
        let t =
            parse_tasks("{\"id\":\"a\",\"mode\":\"find_large\",\"baseline\":{\"items\":\"counted_paths\"}}").unwrap();
        assert_eq!(t[0].baseline.as_ref().unwrap().items, Some(BaselineItems::CountedPaths));
        let t =
            parse_tasks("{\"id\":\"a\",\"mode\":\"read_around\",\"baseline\":{\"items\":\"grep_context\"}}").unwrap();
        assert_eq!(t[0].baseline.as_ref().unwrap().items, Some(BaselineItems::GrepContext));
    }
}
