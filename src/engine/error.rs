//! The one error type of the engine's operations.
//!
//! A variant says what the caller can do about the failure, not where it happened. Each carries the
//! message an agent reads unchanged: the MCP adapter reports it as an `isError` result and the CLI
//! prints it, so a message names the next call to make. A wrapped store, index, git or I/O error is
//! first worded the same way (`EngineError::settled`), its library message going to the log. The
//! adapter maps the variants onto MCP error codes, which the CLI turns into exit codes
//! (`src/mcp/adapter.rs`).

/// Why an engine operation did not answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The arguments cannot be answered as given: a path outside the repository or not in the
    /// index, a stale or foreign cursor, an unknown revision. The caller fixes the call.
    #[error("{0}")]
    InvalidArgument(String),
    /// The operation is not available in this workspace or session: no git repository, a
    /// read-only store asked to write.
    #[error("{0}")]
    Unavailable(String),
    /// The index cannot be read right now (empty, or held by another process); retry later.
    #[error("{0}")]
    IndexUnavailable(String),
    /// Another process holds what the operation reads (the index, the writer lock): retry, or route
    /// the call through the daemon. The CLI exits `3` for it.
    #[error("{0}")]
    Busy(String),
    /// The caller cancelled the operation before it finished.
    #[error("request_cancelled")]
    Cancelled,
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),
    #[error(transparent)]
    Git(#[from] crate::git::GitError),
    #[error(transparent)]
    GitCache(#[from] crate::git_cache::CacheError),
    #[error(transparent)]
    Scan(#[from] crate::scanner::ScanError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A failure inside the engine the caller cannot fix (serialization, an invariant).
    #[error("{0}")]
    Internal(String),
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "the operations moved so far construct only some variants")
)]
impl EngineError {
    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub(crate) fn busy(message: impl Into<String>) -> Self {
        Self::Busy(message.into())
    }

    pub(crate) fn index_unavailable(message: impl Into<String>) -> Self {
        Self::IndexUnavailable(message.into())
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// The refusal of a tool whose retrieval stack is not in this build: it is advertised in every
    /// build, and answers this rather than an empty result an agent would read as "nothing found".
    #[cfg_attr(
        all(feature = "code-search", feature = "documents", feature = "memory"),
        allow(dead_code, reason = "only the gated-off arms name a missing feature")
    )]
    pub(crate) fn feature_missing(tool: &str, feature: &str) -> Self {
        Self::Unavailable(format!(
            "`{tool}` needs the `{feature}` Cargo feature, which is not compiled into this basemind binary. \
             Rebuild with `--features {feature}`; the published release binary includes it."
        ))
    }
}

/// What to do about an index or store that cannot be read.
pub(crate) const INDEX_REPAIR: &str = "run `basemind doctor`; `basemind scan` rebuilds it";

impl EngineError {
    /// This error as one of the variants that carry their message, worded for the agent that reads it:
    /// a store, index, git or I/O failure says what the caller can do, not which library failed or
    /// which file it was reading. The full text of a wrapped error goes to the log.
    pub(crate) fn settled(self) -> Self {
        let settled = match &self {
            Self::Store(error) => Some(store_error(error)),
            Self::Index(error) => Some(index_error(error)),
            Self::Git(error) => Some(git_error(error)),
            Self::GitCache(error) => Some(git_cache_error(error)),
            Self::Scan(error) => Some(scan_error(error)),
            Self::Io(error) => Some(Self::Internal(error.to_string())),
            _ => None,
        };
        let Some(settled) = settled else {
            return self;
        };
        tracing::warn!(error = %self, reported = %settled, "an engine operation failed");
        settled
    }

    /// This error with `context: ` in front of its message.
    pub(crate) fn in_context(self, context: &str) -> Self {
        let prefixed = |message: String| format!("{context}: {message}");
        match self {
            Self::InvalidArgument(message) => Self::InvalidArgument(prefixed(message)),
            Self::Unavailable(message) => Self::Unavailable(prefixed(message)),
            Self::IndexUnavailable(message) => Self::IndexUnavailable(prefixed(message)),
            Self::Busy(message) => Self::Busy(prefixed(message)),
            Self::Internal(message) => Self::Internal(prefixed(message)),
            other => other,
        }
    }
}

/// An index that cannot be read, with what is wrong with it and the repair.
fn unreadable(what: &str) -> EngineError {
    EngineError::IndexUnavailable(format!("{what}: {INDEX_REPAIR}"))
}

fn io_failure(source: &std::io::Error) -> EngineError {
    unreadable(&format!("the index could not be read or written ({})", source.kind()))
}

fn undecodable() -> EngineError {
    unreadable("the index holds a record this basemind cannot decode")
}

/// An index a scan stopped writing to ([`crate::scanner::ScanError::IndexFault`]): the disk is
/// full, or a failed write poisoned the handle. The latched reason quotes the library's error, so
/// the message leaves it out.
fn index_fault(reason: &str) -> EngineError {
    if crate::index::fault::is_storage_full(reason) {
        return EngineError::IndexUnavailable(format!(
            "the index cannot be written because the disk is full; free disk space, then {INDEX_REPAIR}"
        ));
    }
    unreadable("a write to the code index failed, so it takes no more writes")
}

/// The contention of Fjall's own lock on the index database, which carries no holder to name.
fn index_held() -> EngineError {
    EngineError::Busy(
        "another basemind process holds the code index; retry, or start the daemon with \
         `basemind comms start` so it serves every session"
            .to_string(),
    )
}

/// A store failure as the caller reads it.
pub(crate) fn store_error(error: &crate::store::StoreError) -> EngineError {
    use crate::store::StoreError;
    match error {
        StoreError::Io { source, .. } => io_failure(source),
        StoreError::Decode(_) => undecodable(),
        StoreError::SchemaMismatch { .. } => unreadable("the index was written under an older schema"),
        StoreError::StaleBlobLayout { .. } => unreadable("the file map names blobs of an older store layout"),
        StoreError::CorruptBlob { .. } => unreadable("a stored blob is corrupt"),
        StoreError::Decompression { .. } => unreadable("a stored blob does not decompress"),
        StoreError::UnreadableFileMap { .. } => unreadable("the file map is unreadable"),
        StoreError::Locked { .. } => EngineError::Busy(error.to_string()),
        StoreError::NewerSchema { .. } => EngineError::Unavailable(error.to_string()),
        StoreError::ViewNotScanned { .. } => EngineError::IndexUnavailable(error.to_string()),
        StoreError::Index(index) => index_error(index),
        StoreError::Encode(_) | StoreError::BlobTooLarge | StoreError::Compression(_) => {
            EngineError::Internal(error.to_string())
        }
    }
}

/// An inverted-index failure as the caller reads it.
fn index_error(error: &crate::index::IndexError) -> EngineError {
    use crate::index::IndexError;
    match error {
        IndexError::Fjall(fjall::Error::Locked) => index_held(),
        IndexError::Fjall(_) => unreadable("the code index could not be read"),
        IndexError::Corrupt(_) => unreadable("the code index is damaged"),
        IndexError::Io { source, .. } => io_failure(source),
        IndexError::Decode(_) => undecodable(),
        IndexError::StillOpen { .. } => EngineError::IndexUnavailable(
            "another handle on the code index is still open in this process; retry the call".to_string(),
        ),
        IndexError::NewerSchema { .. } | IndexError::NewerMemoryStore { .. } => {
            EngineError::Unavailable(error.to_string())
        }
        IndexError::NeedsRescan { .. } | IndexError::BehindFileMap { .. } => {
            EngineError::IndexUnavailable(error.to_string())
        }
        IndexError::Encode(_) => EngineError::Internal(error.to_string()),
    }
}

/// A git failure as the caller reads it: no repository is an operation this workspace lacks, a path
/// outside it is the caller's to fix.
fn git_error(error: &crate::git::GitError) -> EngineError {
    use crate::git::GitError;
    match error {
        GitError::NotARepo(_) => EngineError::Unavailable("this workspace is not inside a git repository".to_string()),
        GitError::OutsideRepo(_) => EngineError::InvalidArgument(error.to_string()),
        _ => EngineError::Internal(error.to_string()),
    }
}

fn git_cache_error(error: &crate::git_cache::CacheError) -> EngineError {
    use crate::git_cache::CacheError;
    match error {
        CacheError::Io { source, .. } => EngineError::Internal(format!(
            "the git history cache could not be read or written ({}); run `basemind doctor`",
            source.kind()
        )),
        CacheError::Git(error) => git_error(error),
    }
}

/// A scan failure as the caller reads it.
pub(crate) fn scan_error(error: &crate::scanner::ScanError) -> EngineError {
    use crate::scanner::ScanError;
    match error {
        ScanError::Store(error) => store_error(error),
        ScanError::Git(error) => git_error(error),
        ScanError::IndexFault(reason) => index_fault(reason),
        _ => EngineError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use super::*;
    use crate::git::GitError;
    use crate::git_cache::CacheError;
    use crate::index::IndexError;
    use crate::scanner::ScanError;
    use crate::store::StoreError;

    const DATA: &str = "/home/someone/.local/share/basemind/cache/blobs/v34/ab/cdef";

    /// The variant and message an error settles to.
    fn shown(error: EngineError) -> (&'static str, String) {
        let message = error.settled();
        let variant = match &message {
            EngineError::InvalidArgument(_) => "invalid_argument",
            EngineError::Unavailable(_) => "unavailable",
            EngineError::IndexUnavailable(_) => "index_unavailable",
            EngineError::Busy(_) => "busy",
            EngineError::Cancelled => "cancelled",
            EngineError::Internal(_) => "internal",
            other => panic!("a wrapped error must settle to a message variant, got {other:?}"),
        };
        (variant, message.to_string())
    }

    fn unreadable(what: &str) -> (&'static str, String) {
        ("index_unavailable", format!("{what}: {INDEX_REPAIR}"))
    }

    fn denied() -> io::Error {
        io::Error::from(io::ErrorKind::PermissionDenied)
    }

    fn undecodable_record() -> rmp_serde::decode::Error {
        rmp_serde::from_slice::<u8>(&[0xc1]).expect_err("0xc1 is never a msgpack value")
    }

    #[test]
    fn an_unreadable_store_names_the_repair_and_no_path() {
        let cases = [
            (
                StoreError::Io {
                    path: PathBuf::from(DATA),
                    source: denied(),
                },
                unreadable("the index could not be read or written (permission denied)"),
            ),
            (
                StoreError::Decode(undecodable_record()),
                unreadable("the index holds a record this basemind cannot decode"),
            ),
            (
                StoreError::SchemaMismatch { found: 1, expected: 2 },
                unreadable("the index was written under an older schema"),
            ),
            (
                StoreError::StaleBlobLayout { found: 1, expected: 2 },
                unreadable("the file map names blobs of an older store layout"),
            ),
            (
                StoreError::CorruptBlob {
                    path: PathBuf::from(DATA),
                },
                unreadable("a stored blob is corrupt"),
            ),
            (
                StoreError::Decompression {
                    path: PathBuf::from(DATA),
                    source: denied(),
                },
                unreadable("a stored blob does not decompress"),
            ),
            (
                StoreError::UnreadableFileMap {
                    path: PathBuf::from(DATA),
                },
                unreadable("the file map is unreadable"),
            ),
            (
                StoreError::ViewNotScanned {
                    view: "feature".to_string(),
                },
                (
                    "index_unavailable",
                    "view \"feature\" has not been scanned; run `basemind scan --view feature` (or omit \
                     --view to use the working view)"
                        .to_string(),
                ),
            ),
            (
                StoreError::NewerSchema { stored: 9, current: 8 },
                (
                    "unavailable",
                    "index written by a newer basemind (schema 9); this binary understands 8 — upgrade basemind"
                        .to_string(),
                ),
            ),
            (
                StoreError::BlobTooLarge,
                (
                    "internal",
                    "blob payload exceeds the supported envelope limits".to_string(),
                ),
            ),
        ];
        for (error, expected) in cases {
            let what = format!("{error:?}");
            assert_eq!(shown(EngineError::Store(error)), expected, "{what}");
        }
    }

    #[test]
    fn a_held_store_or_index_is_busy() {
        let locked = StoreError::Locked {
            path: PathBuf::from(".lock"),
            holder: None,
        };
        assert_eq!(
            shown(EngineError::Store(locked)),
            (
                "busy",
                "another basemind process holds the lock on .lock (usually the `basemind serve` MCP server \
                 from your editor plugin, or `basemind watch`)"
                    .to_string()
            )
        );
        let held = (
            "busy",
            "another basemind process holds the code index; retry, or start the daemon with `basemind comms \
             start` so it serves every session"
                .to_string(),
        );
        assert_eq!(
            shown(EngineError::Store(StoreError::Index(IndexError::Fjall(
                fjall::Error::Locked
            )))),
            held
        );
        assert_eq!(shown(EngineError::Index(IndexError::Fjall(fjall::Error::Locked))), held);
    }

    #[test]
    fn an_unreadable_index_names_the_repair_and_no_path() {
        let cases = [
            (
                IndexError::Fjall(fjall::Error::Poisoned),
                unreadable("the code index could not be read"),
            ),
            (
                IndexError::Corrupt(fjall::Error::Unrecoverable),
                unreadable("the code index is damaged"),
            ),
            (
                IndexError::Io {
                    path: PathBuf::from(DATA),
                    source: denied(),
                },
                unreadable("the index could not be read or written (permission denied)"),
            ),
            (
                IndexError::Decode(undecodable_record()),
                unreadable("the index holds a record this basemind cannot decode"),
            ),
            (
                IndexError::StillOpen {
                    dir: PathBuf::from(DATA),
                },
                (
                    "index_unavailable",
                    "another handle on the code index is still open in this process; retry the call".to_string(),
                ),
            ),
            (
                IndexError::NeedsRescan { stored: 1, current: 2 },
                (
                    "index_unavailable",
                    "the code index was written under an older schema (1, current 2); rescan to rebuild it".to_string(),
                ),
            ),
            (
                IndexError::NewerMemoryStore { stored: 3, current: 2 },
                (
                    "unavailable",
                    "memory store written by a newer basemind (version 3); this binary understands 2 — upgrade \
                     basemind"
                        .to_string(),
                ),
            ),
        ];
        for (error, expected) in cases {
            let what = format!("{error:?}");
            assert_eq!(shown(EngineError::Index(error)), expected, "{what}");
        }
    }

    #[test]
    fn git_git_cache_scan_and_io_failures_settle_to_a_message_variant() {
        let cases = [
            (
                EngineError::Git(GitError::NotARepo(PathBuf::from("/home/someone/code/repo"))),
                (
                    "unavailable",
                    "this workspace is not inside a git repository".to_string(),
                ),
            ),
            (
                EngineError::Git(GitError::Other("object missing".to_string())),
                ("internal", "git error: object missing".to_string()),
            ),
            (
                EngineError::GitCache(CacheError::Io {
                    path: PathBuf::from(DATA),
                    source: denied(),
                }),
                (
                    "internal",
                    "the git history cache could not be read or written (permission denied); run `basemind doctor`"
                        .to_string(),
                ),
            ),
            (
                EngineError::GitCache(CacheError::Git(GitError::NotARepo(PathBuf::from("/x")))),
                (
                    "unavailable",
                    "this workspace is not inside a git repository".to_string(),
                ),
            ),
            (
                EngineError::Scan(ScanError::Store(StoreError::CorruptBlob {
                    path: PathBuf::from(DATA),
                })),
                unreadable("a stored blob is corrupt"),
            ),
            (
                EngineError::Scan(ScanError::BadGlob("[".to_string())),
                ("internal", "invalid glob in config: [".to_string()),
            ),
            (
                EngineError::Scan(ScanError::IndexFault(format!(
                    "disk full: io error on {DATA}: no space left"
                ))),
                (
                    "index_unavailable",
                    format!(
                        "the index cannot be written because the disk is full; free disk space, then {INDEX_REPAIR}"
                    ),
                ),
            ),
            (
                EngineError::Scan(ScanError::IndexFault(
                    "index handle poisoned: fjall error: Poisoned".to_string(),
                )),
                unreadable("a write to the code index failed, so it takes no more writes"),
            ),
            (
                EngineError::Io(io::Error::other("disk gone")),
                ("internal", "disk gone".to_string()),
            ),
        ];
        for (error, expected) in cases {
            let what = format!("{error:?}");
            assert_eq!(shown(error), expected, "{what}");
        }
    }

    #[test]
    fn a_message_variant_is_already_settled() {
        assert_eq!(
            shown(EngineError::invalid_argument("bad path")),
            ("invalid_argument", "bad path".to_string())
        );
        assert_eq!(
            shown(EngineError::Cancelled),
            ("cancelled", "request_cancelled".to_string())
        );
    }

    #[test]
    fn a_rescan_that_cannot_write_the_index_names_the_repair_and_no_path() {
        let full = ScanError::IndexFault(format!("disk full: io error on {DATA}: no space left"));
        assert_eq!(
            shown(crate::engine::scan::rescan_error(&full)),
            (
                "index_unavailable",
                format!(
                    "rescan: the index cannot be written because the disk is full; free disk space, then \
                     {INDEX_REPAIR}"
                ),
            )
        );
    }

    #[test]
    fn a_context_prefixes_the_message() {
        let error = store_error(&StoreError::CorruptBlob {
            path: PathBuf::from(DATA),
        })
        .in_context("rescan");
        assert_eq!(
            error.to_string(),
            format!("rescan: a stored blob is corrupt: {INDEX_REPAIR}")
        );
    }
}
