//! The contract every arm implements, so the measurement loop and the ground truth never know which
//! store answered.

use std::sync::Arc;

use anyhow::Result;
use lancedb::arrow::arrow_array::RecordBatch;

/// A chunk identity, rendered `"<doc_key>:<ord>"` for `basemind::eval::score`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hit {
    pub doc_key: String,
    pub ord: u32,
}

impl Hit {
    pub fn render(&self) -> String {
        format!("{}:{}", self.doc_key, self.ord)
    }
}

/// The predicate applied before ranking in every arm: the request's metadata filter and, when the
/// worktree's live-key list is short enough to be a predicate (ADR-0029 rule 3), that list.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub group: Option<&'static str>,
    pub keys: Option<Arc<Vec<String>>>,
}

impl Filter {
    /// The metadata filter that keeps `percent` of the corpus (see `corpus::mime_group`).
    pub fn for_selectivity(percent: u32) -> Result<Self> {
        let group = match percent {
            100 => None,
            10 => Some("pdf"),
            1 => Some("email"),
            other => anyhow::bail!("selectivity must be 1, 10 or 100, got {other}"),
        };
        Ok(Self { group, keys: None })
    }

    pub fn with_keys(mut self, keys: Vec<String>) -> Self {
        self.keys = Some(Arc::new(keys));
        self
    }

    pub fn group_sql(&self) -> Option<String> {
        self.group.map(|group| format!("mime_group = '{group}'"))
    }

    /// The whole predicate as SQL, key list included; only `count_rows` needs this form.
    pub fn sql(&self) -> Option<String> {
        let keys = self.keys.as_ref().map(|keys| {
            let list: Vec<String> = keys.iter().map(|key| format!("'{key}'")).collect();
            format!("doc_key IN ({})", list.join(", "))
        });
        match (self.group_sql(), keys) {
            (Some(group), Some(keys)) => Some(format!("{group} AND {keys}")),
            (group, keys) => group.or(keys),
        }
    }
}

/// How a vector query was answered; reported so a run states which path produced its numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMode {
    Exact,
    Ann,
}

pub trait Backend {
    /// Ingest rows without building any secondary structure the arm treats as an index.
    fn build(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()>;

    fn build_indexes(&mut self) -> Result<()>;

    /// The `k` nearest chunks by L2 among the rows `filter` keeps, nearest first.
    fn vector(&self, query: &[f32], filter: &Filter, k: usize) -> Result<(Vec<Hit>, VectorMode)>;

    /// The `k` best BM25 chunks among the rows `filter` keeps; `None` when the arm has no lexical lane.
    fn lexical(&self, text: &str, filter: &Filter, k: usize) -> Result<Option<Vec<Hit>>>;

    /// Add rows the way a rescan would, leaving any index as it was.
    fn append(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()>;

    /// Fold appended rows into the arm's indexes.
    fn optimize(&mut self) -> Result<()>;
}
