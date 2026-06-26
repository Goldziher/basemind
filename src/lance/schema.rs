//! Arrow schemas for the LanceDB-backed `documents` and `memory` tables.
//!
//! The vector dimension is fixed once at table-creation time; mismatched dims
//! trigger a wipe-and-rebuild (see [`crate::lance::LanceStore::open`]).

use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};

/// Build the schema for the per-document-chunk `documents` table.
///
/// Columns:
/// - `scope`     UTF-8     repo identity (normalised git remote URL or workdir path)
/// - `path`      UTF-8     repo-relative path of the source file
/// - `chunk_idx` UInt32    0-based index of this chunk within the file
/// - `mime_type` UTF-8     IANA MIME type kreuzberg detected
/// - `text`      UTF-8     the chunk text (snippet returned by search results)
/// - `byte_start` UInt32   chunk start byte offset in the original document
/// - `byte_end`  UInt32    chunk end byte offset
/// - `embedding` FixedSizeList<Float32, DIM>  the embedding vector
pub fn documents_schema(dim: u16) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("scope", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
        Field::new("chunk_idx", DataType::UInt32, false),
        Field::new("mime_type", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("byte_start", DataType::UInt32, false),
        Field::new("byte_end", DataType::UInt32, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                i32::from(dim),
            ),
            false,
        ),
    ]))
}

/// Build the schema for the `memory` table.
///
/// Columns:
/// - `scope`       UTF-8     repo identity
/// - `key`         UTF-8     primary lookup key (unique within `(scope, visibility, agent_id)`)
/// - `value`       UTF-8     the stored value text
/// - `tags`        `List<UTF-8>`  optional tags
/// - `visibility`  UTF-8     memory tier: `"group"` (shared) or `"individual"` (per-agent)
/// - `agent_id`    UTF-8     owner of an individual-tier row (empty for the group tier)
/// - `embedding`   FixedSizeList<Float32, DIM>
/// - `created_at`  TimestampMicros
/// - `updated_at`  TimestampMicros
pub fn memory_schema(dim: u16) -> SchemaRef {
    let tags_inner = Arc::new(Field::new("item", DataType::Utf8, true));
    Arc::new(Schema::new(vec![
        Field::new("scope", DataType::Utf8, false),
        Field::new("key", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
        Field::new("tags", DataType::List(tags_inner), true),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("agent_id", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                i32::from(dim),
            ),
            false,
        ),
        Field::new(
            "created_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            false,
        ),
        Field::new(
            "updated_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            false,
        ),
    ]))
}

/// Table names — small constants in one place so the `LanceStore` impl and any
/// future migration code agree on what's where.
pub const DOCUMENTS_TABLE: &str = "documents";
pub const MEMORY_TABLE: &str = "memory";
