//! Task-file model: one JSON object per line.
//!
//! ```json
//! {"id":"sym-1","mode":"symbols","args":{"name":"Widget"},"gold":["src/w.py:12"],
//!  "scoring":"set","k":10,"baseline":{"grep":"git grep -n Widget","read":["src/w.py"]}}
//! ```

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Retrieval modes the harness can drive. Each maps onto exactly one MCP tool mode.
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
}

impl EvalMode {
    /// The `domain:mode` telemetry key of the tool call this mode drives; also the key
    /// `savings::estimate_from_text` classifies on.
    pub fn tool_key(self) -> &'static str {
        match self {
            Self::Symbols => "code:symbols",
            Self::Outline => "code:outline",
            Self::References => "code:references",
            Self::Callers => "code:callers",
            Self::Grep => "code:grep",
            Self::Find => "code:find",
            Self::Dependents => "code:dependents",
            Self::GitSearch => "git:search",
            Self::Docs => "memory:documents",
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Task {
    pub id: String,
    pub mode: EvalMode,
    /// Tool arguments for the mode (`name`, `pattern`, `path`, `query`, ...), exactly as the MCP
    /// tool takes them, minus `mode`.
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
}
