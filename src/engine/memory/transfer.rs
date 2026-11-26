//! `basemind memory export` and `basemind memory import` (ADR-0056): the notes of one tier of the
//! repository's current scope as a file, and a file merged back into a repository.
//!
//! The file is JSON lines. The first line is a header naming the format, its version, the tier and
//! the number of notes; every other line is one note, whole, ordered by key, so the same notes always
//! export to the same bytes. An import validates the whole file before it writes anything.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::ops::{self, Merged, WireMemoryEntry};
use super::vectors;
use crate::engine::EngineError;
use crate::engine::types::memory::{
    MemoryExportParams, MemoryExportResponse, MemoryImportParams, MemoryImportResponse, TRANSFER_MAX_BYTES, Visibility,
};
use crate::engine::workspace::Workspace;

/// The `format` a header names.
pub(crate) const FORMAT: &str = "basemind-memory-export";

/// The layout of the file this binary writes and the newest it reads.
pub(crate) const VERSION: u32 = 1;

/// The most notes one file may hold.
pub(crate) const TRANSFER_MAX_NOTES: usize = 100_000;

/// The most tags one note may carry in a file.
pub(crate) const NOTE_MAX_TAGS: usize = 256;

/// The longest `source` label, in characters.
pub(crate) const SOURCE_MAX_CHARS: usize = 256;

/// The source recorded when the caller names none.
const DEFAULT_SOURCE: &str = "file";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format: String,
    version: u32,
    visibility: Visibility,
    notes: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    key: String,
    value: String,
    tags: Vec<String>,
    created_at: i64,
    updated_at: i64,
}

impl From<WireMemoryEntry> for Line {
    fn from(entry: WireMemoryEntry) -> Self {
        Self {
            key: entry.key,
            value: entry.value,
            tags: entry.tags,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }
}

impl From<Line> for WireMemoryEntry {
    fn from(line: Line) -> Self {
        Self {
            key: line.key,
            value: line.value,
            tags: line.tags,
            created_at: line.created_at,
            updated_at: line.updated_at,
        }
    }
}

/// The export file's text for `entries` of `visibility`.
fn render(entries: Vec<WireMemoryEntry>, visibility: Visibility) -> Result<String, EngineError> {
    let header = Header {
        format: FORMAT.to_string(),
        version: VERSION,
        visibility,
        notes: entries.len(),
    };
    let mut text = serde_json::to_string(&header).map_err(encode_error)?;
    text.push('\n');
    for entry in entries {
        text.push_str(&serde_json::to_string(&Line::from(entry)).map_err(encode_error)?);
        text.push('\n');
    }
    Ok(text)
}

fn encode_error(error: serde_json::Error) -> EngineError {
    EngineError::Internal(format!("encode memory export: {error}"))
}

/// The tier and notes of an export file, or why the file is not one. Every note is checked against
/// the limits of a save; nothing is written by this function.
fn parse(content: &str) -> Result<(Visibility, Vec<WireMemoryEntry>), EngineError> {
    let bad =
        |line: usize, problem: String| EngineError::invalid_argument(format!("memory import: line {line}: {problem}"));
    if content.len() > TRANSFER_MAX_BYTES {
        return Err(EngineError::invalid_argument(format!(
            "memory import: the file has {} bytes and at most {TRANSFER_MAX_BYTES} are read; split it with \
             `memory export --prefix` or `--tag`",
            content.len()
        )));
    }
    let mut lines = content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .filter(|(_, line)| !line.is_empty());
    let (number, first) = lines
        .next()
        .ok_or_else(|| EngineError::invalid_argument("memory import: the file is empty"))?;
    let header: Header =
        serde_json::from_str(first).map_err(|error| bad(number, format!("not an export header: {error}")))?;
    if header.format != FORMAT {
        return Err(bad(number, format!("format `{}`, expected `{FORMAT}`", header.format)));
    }
    if header.version > VERSION {
        return Err(bad(
            number,
            format!(
                "version {} was written by a newer basemind; this one reads up to {VERSION}: upgrade basemind",
                header.version
            ),
        ));
    }
    if header.version != VERSION {
        return Err(bad(number, format!("unsupported version {}", header.version)));
    }
    if header.notes > TRANSFER_MAX_NOTES {
        return Err(bad(
            number,
            format!("{} notes, at most {TRANSFER_MAX_NOTES} are read", header.notes),
        ));
    }
    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(header.notes);
    for (number, text) in lines {
        let line: Line = serde_json::from_str(text).map_err(|error| bad(number, format!("not a note: {error}")))?;
        let entry = WireMemoryEntry::from(line);
        check_entry(&entry).map_err(|problem| bad(number, problem))?;
        if !seen.insert(entry.key.clone()) {
            return Err(bad(number, format!("key `{}` appears twice", entry.key)));
        }
        entries.push(entry);
        if entries.len() > header.notes {
            return Err(bad(
                number,
                format!("more notes than the {} the header names", header.notes),
            ));
        }
    }
    if entries.len() != header.notes {
        return Err(EngineError::invalid_argument(format!(
            "memory import: the header names {} notes and the file has {}; it is truncated or edited",
            header.notes,
            entries.len()
        )));
    }
    Ok((header.visibility, entries))
}

/// Why `entry` cannot be imported, or `Ok`.
fn check_entry(entry: &WireMemoryEntry) -> Result<(), String> {
    if entry.key.is_empty() || entry.key.contains('\0') {
        return Err("a key is empty or holds a NUL byte".to_string());
    }
    if entry.created_at < 0 || entry.updated_at < 0 {
        return Err(format!("key `{}` has a negative timestamp", entry.key));
    }
    if entry.tags.len() > NOTE_MAX_TAGS {
        return Err(format!(
            "key `{}` has {} tags, at most {NOTE_MAX_TAGS} are read",
            entry.key,
            entry.tags.len()
        ));
    }
    ops::check_note(&entry.key, &entry.value, &entry.tags).map_err(|error| error.to_string())
}

/// The label recorded as the provenance of imported notes.
fn source_label(source: Option<&str>) -> Result<String, EngineError> {
    let label = source
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SOURCE);
    if label.chars().count() > SOURCE_MAX_CHARS || label.chars().any(char::is_control) {
        return Err(EngineError::invalid_argument(format!(
            "memory import: the source label is longer than {SOURCE_MAX_CHARS} characters or holds a control character"
        )));
    }
    Ok(label.to_string())
}

impl Workspace {
    /// The notes of one tier of the current scope as an export file, optionally narrowed by key
    /// prefix and tags.
    pub(crate) fn memory_export(&self, params: MemoryExportParams) -> Result<MemoryExportResponse, EngineError> {
        let (vis_byte, owner) = self.memory_namespace(params.visibility);
        let entries = {
            let store = self.store_read();
            let idx = self.memory_index(&store)?;
            ops::scan_core(
                &idx,
                &self.scope,
                vis_byte,
                owner,
                params.prefix.as_deref().unwrap_or_default(),
                &params.tags,
                TRANSFER_MAX_BYTES,
            )?
        };
        let notes = entries.len();
        Ok(MemoryExportResponse {
            content: render(entries, params.visibility)?,
            notes,
        })
    }

    /// Merge an export file into one tier of the current scope by the request's policy. The file is
    /// validated whole first; a file of another tier is refused.
    pub(crate) fn memory_import(&self, params: MemoryImportParams) -> Result<MemoryImportResponse, EngineError> {
        let source = source_label(params.source.as_deref())?;
        let (file_tier, entries) = parse(&params.content)?;
        if file_tier != params.visibility {
            return Err(EngineError::invalid_argument(format!(
                "memory import: the file holds {file_tier:?} notes and the target tier is {:?}; pass or drop \
                 `--individual` to match",
                params.visibility
            )));
        }
        let (vis_byte, owner) = self.memory_namespace(params.visibility);
        let mut report = MemoryImportResponse {
            total: entries.len(),
            ..MemoryImportResponse::default()
        };
        let mut replaced = Vec::new();
        {
            let store = self.store_read();
            let idx = self.memory_index(&store)?;
            for entry in &entries {
                let key_lock = super::memory_put_lock(&self.scope, vis_byte, owner, &entry.key);
                let _guard = key_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                match ops::merge_core(&idx, &self.scope, vis_byte, owner, entry, params.policy, &source)? {
                    Merged::Added => report.added += 1,
                    Merged::Replaced => {
                        report.replaced += 1;
                        replaced.push(entry.key.as_str());
                    }
                    Merged::Kept => report.kept += 1,
                    Merged::Unchanged => report.unchanged += 1,
                }
            }
        }
        if report.added + report.replaced > 0 {
            // A replaced note's old vector row holds the old text; the next search by meaning embeds
            // the written notes, as after a save that could not embed.
            for key in replaced {
                vectors::delete_vector_row(self, params.visibility, owner, key);
            }
            vectors::not_level(self, vis_byte, owner);
            vectors::owe_rows(&self.root);
        }
        Ok(report)
    }
}

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod transfer_tests;
