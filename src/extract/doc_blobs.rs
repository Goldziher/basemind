//! [`DocBlobs`]: the document-blob accessors over the store, which owns the bytes but not the
//! document type.

use super::FileMapDoc;
use crate::hashing::Hash;
use crate::store::{Store, StoreError};

/// Document blobs in the content-addressed store. The store writes and reads them generically
/// (`Store::write_doc_blob` / `Store::read_doc_blob_by_hex`); this names the type, which belongs to
/// the retrieval layer.
pub trait DocBlobs {
    /// Write a document blob, always overwriting (see `Store::write_doc_blob`).
    fn write_doc(&self, hash: &Hash, map: &FileMapDoc) -> Result<(), StoreError>;

    /// Read the document blob of `hash_hex`; `None` when there is none.
    fn read_doc_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapDoc>, StoreError>;
}

impl DocBlobs for Store {
    fn write_doc(&self, hash: &Hash, map: &FileMapDoc) -> Result<(), StoreError> {
        self.write_doc_blob(hash, map)
    }

    fn read_doc_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapDoc>, StoreError> {
        self.read_doc_blob_by_hex(hash_hex, |doc: &FileMapDoc| doc.schema_ver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::SCHEMA_VER;

    /// Issue #44: a Deferred pass persists the doc blob vectorless (`embedding_dim: 0`); the later
    /// Inline pass re-extracts + embeds and writes the SAME content hash again. That second write
    /// must replace the blob — a schema-only skip keeps it vectorless forever, and every future
    /// entry-less encounter of the content re-embeds again (the re-embed loop).
    #[test]
    fn write_doc_overwrites_vectorless_blob_with_embedded_doc() {
        use crate::hashing;
        use crate::store::{Store, VIEW_WORKING, init_isolated_cache};

        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash = crate::hashing::hash_bytes(b"bug-44 deferred-then-inline doc");

        let vectorless = FileMapDoc {
            config_digest: String::new(),
            linear_chunked: false,
            text_fallback: false,
            schema_ver: SCHEMA_VER,
            mime_type: "text/plain".to_string(),
            content: "hello".to_string(),
            metadata: Vec::new(),
            detected_languages: Vec::new(),
            chunks: Vec::new(),
            embedding_model: String::new(),
            embedding_dim: 0,
            keywords: Vec::new(),
            entities: Vec::new(),
            summary: None,
            language_confidences: Vec::new(),
        };
        store.write_doc(&hash, &vectorless).expect("write vectorless blob");

        let embedded = FileMapDoc {
            config_digest: String::new(),
            embedding_model: "balanced".to_string(),
            embedding_dim: 768,
            ..vectorless
        };
        store.write_doc(&hash, &embedded).expect("write embedded blob");

        let hex_buf = hashing::hex_buf(&hash);
        let path = store.blob_path_doc_hex(hashing::hex_str(&hex_buf));
        let persisted = std::fs::read(path).expect("read doc blob bytes");
        assert_eq!(&persisted[..4], b"BMB1", "new document blobs carry the envelope magic");
        let read = store
            .read_doc_by_hex(hashing::hex_str(&hex_buf))
            .expect("read doc blob")
            .expect("doc blob present");
        assert_eq!(
            read.embedding_dim, 768,
            "Inline pass's embedded doc must replace the Deferred pass's vectorless blob (issue #44)"
        );
    }
}
