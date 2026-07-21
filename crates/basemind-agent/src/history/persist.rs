//! JSONL session persistence and resume.
//!
//! Each session owns a directory under the machine-global cache, namespaced by repo so "resume
//! latest" finds this repo's newest session:
//! `cache_root()/agent/sessions/<workspace_key>/<session_id>/`. The directory holds two files:
//!
//! - `messages.jsonl` — one JSON-encoded [`liter_llm::Message`] per line, appended between turns.
//! - `meta.json` — a pretty-printed [`SessionMeta`] snapshot (cwd, title, cumulative token totals).
//!
//! Session ids are zero-padded epoch-millis so directory names sort lexically in creation order;
//! [`SessionStore::latest_id`] therefore just takes the maximum name.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use liter_llm::Message;
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::error::{AgentError, Result};

/// Filename of the append-only message log inside a session directory.
const MESSAGES_FILE: &str = "messages.jsonl";
/// Filename of the session metadata snapshot inside a session directory.
const META_FILE: &str = "meta.json";
/// Width of a zero-padded epoch-millis session id (fits `u64::MAX` millis with room to spare).
const SESSION_ID_WIDTH: usize = 20;

/// A snapshot of a session's identity and cumulative accounting, persisted to `meta.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The session id (also the directory name).
    pub id: String,
    /// The repo root the session was started in, as a display string.
    pub cwd: String,
    /// A short human-facing title (derived from the first user message), if known.
    pub title: Option<String>,
    /// Cumulative input tokens across all persisted turns.
    pub input_tokens: u64,
    /// Cumulative output tokens across all persisted turns.
    pub output_tokens: u64,
}

/// A handle to one session's on-disk directory: the message log plus its metadata snapshot.
#[derive(Clone, Debug)]
pub struct SessionStore {
    dir: PathBuf,
    id: String,
}

/// The per-repo session area: `cache_root()/agent/sessions/<workspace_key>/`.
fn sessions_area(root: &Path) -> PathBuf {
    basemind::store_layout::cache_root()
        .join("agent")
        .join("sessions")
        .join(basemind::store_layout::workspace_key(root))
}

/// Current wall-clock time as epoch milliseconds.
fn epoch_millis() -> Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .map_err(|error| AgentError::Config(format!("system clock before unix epoch: {error}")))
}

impl SessionStore {
    /// Create a fresh session directory for `root` and return a handle to it.
    ///
    /// The id is zero-padded epoch-millis, so later sessions sort after earlier ones and
    /// [`SessionStore::latest_id`] can select the newest by string maximum.
    pub async fn create(root: &Path) -> Result<Self> {
        let id = format!("{:0width$}", epoch_millis()?, width = SESSION_ID_WIDTH);
        let dir = sessions_area(root).join(&id);
        fs::create_dir_all(&dir)
            .await
            .map_err(|error| with_context("create session dir", error))?;
        Ok(Self { dir, id })
    }

    /// Open an existing session by id, returning the store, its restored messages, and its metadata.
    ///
    /// A trailing blank line in `messages.jsonl` is tolerated.
    pub async fn open(root: &Path, id: &str) -> Result<(Self, Vec<Message>, SessionMeta)> {
        let dir = sessions_area(root).join(id);
        let messages_path = dir.join(MESSAGES_FILE);
        let raw = fs::read_to_string(&messages_path)
            .await
            .map_err(|error| with_context("read session messages", error))?;
        let mut messages = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            messages.push(serde_json::from_str::<Message>(line)?);
        }
        let meta_raw = fs::read_to_string(dir.join(META_FILE))
            .await
            .map_err(|error| with_context("read session meta", error))?;
        let meta = serde_json::from_str::<SessionMeta>(&meta_raw)?;
        Ok((
            Self {
                dir,
                id: id.to_string(),
            },
            messages,
            meta,
        ))
    }

    /// The id of the newest session for `root`, or `None` when this repo has no sessions yet.
    pub async fn latest_id(root: &Path) -> Result<Option<String>> {
        let area = sessions_area(root);
        let mut entries = match fs::read_dir(&area).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(with_context("list sessions", error)),
        };
        let mut latest: Option<String> = None;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| with_context("list sessions", error))?
        {
            if !entry.file_type().await.map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if latest.as_ref().is_none_or(|current| name > *current) {
                latest = Some(name);
            }
        }
        Ok(latest)
    }

    /// This session's id (also its directory name).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Append `messages` to `messages.jsonl`, one JSON object per line.
    pub async fn append(&self, messages: &[Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut buffer = String::new();
        for message in messages {
            buffer.push_str(&serde_json::to_string(message)?);
            buffer.push('\n');
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(MESSAGES_FILE))
            .await
            .map_err(|error| with_context("open session messages", error))?;
        file.write_all(buffer.as_bytes())
            .await
            .map_err(|error| with_context("append session messages", error))?;
        Ok(())
    }

    /// Overwrite `meta.json` with `meta` (pretty-printed).
    pub async fn write_meta(&self, meta: &SessionMeta) -> Result<()> {
        let encoded = serde_json::to_string_pretty(meta)?;
        fs::write(self.dir.join(META_FILE), encoded)
            .await
            .map_err(|error| with_context("write session meta", error))?;
        Ok(())
    }
}

/// Wrap an IO error with a short description of the operation that failed.
fn with_context(operation: &str, error: std::io::Error) -> AgentError {
    AgentError::Io(std::io::Error::new(error.kind(), format!("{operation}: {error}")))
}

#[cfg(test)]
mod tests {
    use liter_llm::{UserContent, UserMessage};
    use tempfile::TempDir;

    use super::*;

    /// A test harness that isolates `BASEMIND_DATA_HOME` and the repo root to unique temp dirs.
    ///
    /// `BASEMIND_DATA_HOME` is process-global, so these tests serialize on a mutex to stay
    /// independent under the default parallel test runner.
    struct Harness {
        _data_home: TempDir,
        root: TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        &LOCK
    }

    impl Harness {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap_or_else(|poison| poison.into_inner());
            let data_home = tempfile::tempdir().expect("data home tempdir");
            // SAFETY (test-only): the env mutation is serialized by `env_lock` above, so no other ~keep
            // test observes a half-set value. ~keep
            unsafe { std::env::set_var("BASEMIND_DATA_HOME", data_home.path()) };
            Self {
                _data_home: data_home,
                root: tempfile::tempdir().expect("root tempdir"),
                _guard: guard,
            }
        }

        fn root(&self) -> &Path {
            self.root.path()
        }
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            name: None,
        })
    }

    fn user_text(message: &Message) -> Option<&str> {
        match message {
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) => Some(text.as_str()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn create_append_open_round_trips_messages_and_meta() {
        let harness = Harness::new();
        let store = SessionStore::create(harness.root()).await.expect("create");
        store.append(&[user("first"), user("second")]).await.expect("append");
        let meta = SessionMeta {
            id: store.id().to_string(),
            cwd: harness.root().display().to_string(),
            title: Some("first".into()),
            input_tokens: 100,
            output_tokens: 20,
        };
        store.write_meta(&meta).await.expect("write meta");

        let (reopened, messages, restored_meta) = SessionStore::open(harness.root(), store.id()).await.expect("open");
        assert_eq!(reopened.id(), store.id());
        assert_eq!(messages.len(), 2);
        assert_eq!(user_text(&messages[0]), Some("first"));
        assert_eq!(user_text(&messages[1]), Some("second"));
        assert_eq!(messages, vec![user("first"), user("second")]);
        assert_eq!(restored_meta.id, meta.id);
        assert_eq!(restored_meta.cwd, meta.cwd);
        assert_eq!(restored_meta.title.as_deref(), Some("first"));
        assert_eq!(restored_meta.input_tokens, 100);
        assert_eq!(restored_meta.output_tokens, 20);
    }

    #[tokio::test]
    async fn append_is_additive_across_calls() {
        let harness = Harness::new();
        let store = SessionStore::create(harness.root()).await.expect("create");
        store.append(&[user("one")]).await.expect("append one");
        store.append(&[user("two")]).await.expect("append two");
        // open() needs meta.json alongside the log; write one so the read succeeds. ~keep
        store
            .write_meta(&SessionMeta {
                id: store.id().to_string(),
                cwd: String::new(),
                title: None,
                input_tokens: 0,
                output_tokens: 0,
            })
            .await
            .expect("write meta");
        let (_, messages, _) = SessionStore::open(harness.root(), store.id()).await.expect("open");
        assert_eq!(messages, vec![user("one"), user("two")]);
    }

    #[tokio::test]
    async fn latest_id_returns_none_then_newest() {
        let harness = Harness::new();
        assert_eq!(
            SessionStore::latest_id(harness.root()).await.expect("latest empty"),
            None
        );
        let first = SessionStore::create(harness.root()).await.expect("first");
        assert_eq!(
            SessionStore::latest_id(harness.root())
                .await
                .expect("latest one")
                .as_deref(),
            Some(first.id())
        );
        // A later id sorts after the first (ids are zero-padded epoch-millis); force a strictly ~keep
        // greater id so the assertion holds even inside the same millisecond. ~keep
        let newer_id = format!("{:0width$}", u128::MAX, width = SESSION_ID_WIDTH);
        let newer_dir = sessions_area(harness.root()).join(&newer_id);
        fs::create_dir_all(&newer_dir).await.expect("newer dir");
        assert_eq!(
            SessionStore::latest_id(harness.root())
                .await
                .expect("latest two")
                .as_deref(),
            Some(newer_id.as_str())
        );
    }

    #[tokio::test]
    async fn open_tolerates_a_trailing_blank_line() {
        let harness = Harness::new();
        let store = SessionStore::create(harness.root()).await.expect("create");
        store.append(&[user("only")]).await.expect("append");
        // Simulate a trailing blank line beyond the newline `append` already writes. ~keep
        store.append(&[]).await.expect("noop append");
        let path = sessions_area(harness.root()).join(store.id()).join(MESSAGES_FILE);
        let mut raw = fs::read_to_string(&path).await.expect("read");
        raw.push('\n');
        fs::write(&path, raw).await.expect("rewrite");
        store
            .write_meta(&SessionMeta {
                id: store.id().to_string(),
                cwd: String::new(),
                title: None,
                input_tokens: 0,
                output_tokens: 0,
            })
            .await
            .expect("write meta");
        let (_, messages, _) = SessionStore::open(harness.root(), store.id()).await.expect("open");
        assert_eq!(messages, vec![user("only")]);
    }
}
