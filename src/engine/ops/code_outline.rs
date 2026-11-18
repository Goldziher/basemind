//! `code_outline`: one file's symbols with their 1-based line spans, plus its imports.

use std::sync::atomic::Ordering;

use super::SEARCH_LIMIT_MAX;
use crate::engine::budget::{apply_budget, budget};
use crate::engine::cursor::Cursor;
use crate::engine::error::EngineError;
use crate::engine::files::{indexed_file, not_indexed_in};
use crate::engine::freshness;
use crate::engine::timing::elapsed_us;
use crate::engine::types::code::{CodeOutlineParams, OutlineResponse, OutlineSymbol, kind_to_str};
use crate::engine::workspace::Workspace;

/// Symbols returned by one `code_outline` page when the caller passes no `limit`.
const OUTLINE_LIMIT_DEFAULT: u32 = 500;
/// Imports listed on the first `code_outline` page.
const OUTLINE_IMPORTS_MAX: usize = 200;

/// 1-based line of the byte at `offset`, from the sorted offsets of every newline in the file.
fn line_of(newlines: &[usize], offset: usize) -> u32 {
    newlines.partition_point(|newline| *newline < offset) as u32 + 1
}

impl Workspace {
    /// `code_outline`: one file's symbols with their 1-based line spans, plus its imports.
    ///
    /// `end_line` is not in the index (a symbol records its byte span and start row), so it is
    /// derived from the working-tree file. When that file no longer holds the indexed content the
    /// outline is extracted from the file as it is and flagged `stale`: the indexed spans laid over
    /// other bytes would end on the wrong lines.
    pub(crate) fn code_outline(&self, p: CodeOutlineParams) -> Result<OutlineResponse, EngineError> {
        let started = std::time::Instant::now();
        let generation = self.index_generation.load(Ordering::Relaxed);
        // Path-keyed: served from the in-RAM map when it is warm, else from the one blob. Waiting for
        // the map here would make a one-shot CLI call decode the whole corpus to outline one file.
        let cache = self.cache.load_full();
        let (file, entry, l1) = {
            let store = self.store_read();
            let (entry, l1) = freshness::indexed_outline(&store, &cache, &p.path)
                .ok_or_else(|| not_indexed_in(&store, &self.root, "code_outline", &p.path))?;
            (indexed_file(self, &store, "code_outline", &p.path)?, entry, l1)
        };

        let max_bytes = self.config.scan.max_file_bytes;
        let live = freshness::load(&file, (&entry).into(), max_bytes)
            .map_err(|reason| freshness::unloadable("code_outline", &p.path, reason, max_bytes))?;
        // A page of an edited file is a page of what it holds now, which no index generation
        // describes: its cursor is bound to those bytes too.
        let snapshot = if live.fresh {
            generation
        } else {
            let digest = crate::hashing::hash_bytes(&live.bytes);
            generation ^ u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
        };
        let skip = match p.cursor.as_ref() {
            Some(cursor) => {
                let (offset, issued) = cursor.decode_in_memory()?;
                if issued != snapshot {
                    return Err(EngineError::invalid_argument(if live.fresh {
                        "`code_outline`: the index changed since this cursor was issued. Call again without `cursor`."
                    } else {
                        "`code_outline`: the file changed since this cursor was issued. Call again without `cursor`."
                    }));
                }
                offset as usize
            }
            None => 0,
        };
        let limit = p.limit.unwrap_or(OUTLINE_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;

        let l1 = if live.fresh {
            l1
        } else {
            freshness::reparse(self, "code_outline", &p.path, &live.bytes)?
        };
        let (source, stale) = (live.bytes, !live.fresh);
        let newlines: Vec<usize> = memchr::memchr_iter(b'\n', &source).collect();

        let total = l1.symbols.len();
        let page: Vec<OutlineSymbol> = l1
            .symbols
            .iter()
            .skip(skip)
            .take(limit)
            .map(|symbol| {
                let line = symbol.start_row + 1;
                let end_line = line_of(&newlines, (symbol.end_byte as usize).min(source.len())).max(line);
                OutlineSymbol {
                    name: symbol.name.clone(),
                    kind: kind_to_str(symbol.kind),
                    line,
                    end_line,
                    signature: symbol.signature.clone(),
                }
            })
            .collect();
        let budgeted = apply_budget(page, budget(p.max_tokens));
        let symbols = budgeted.items;
        let next = skip + symbols.len();
        let imports: Vec<String> = if skip == 0 {
            l1.imports
                .iter()
                .take(OUTLINE_IMPORTS_MAX)
                .map(|import| import.module.clone().unwrap_or_else(|| import.raw.clone()))
                .collect()
        } else {
            Vec::new()
        };

        let imports_omitted = if skip == 0 {
            l1.imports.len().saturating_sub(OUTLINE_IMPORTS_MAX)
        } else {
            0
        };

        Ok(OutlineResponse {
            path: p.path,
            language: l1.language.clone(),
            size_bytes: l1.size_bytes,
            had_errors: l1.had_errors,
            total,
            budgeted: budgeted.budgeted,
            stale,
            symbols,
            truncated: imports_omitted > 0,
            truncated_reason: (imports_omitted > 0).then_some("imports"),
            omitted: imports_omitted,
            imports,
            next_cursor: (next < total).then(|| Cursor::encode_in_memory(next as u64, snapshot)),
            notice: self.lifecycle_notice(),
            elapsed_us: elapsed_us(started),
        })
    }
}
