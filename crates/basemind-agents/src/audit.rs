//! The append-only record of every launch decision a [`Supervisor`](crate::Supervisor) makes.
//!
//! A record says who asked, for which harness, where, at what depth and under which limits,
//! and what came of it. It never carries a brief or follow-up, an approval's command or
//! detail, an environment value, a launch token or a harness's error text: the outcome is a
//! fixed code, so nothing a model or a harness wrote reaches the log.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// What a record is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditEvent {
    /// A launch passed the policy and is about to start.
    Launch,
    /// A launch was refused, by the policy or by the adapter.
    Refusal,
    /// A caller asked a session to stop.
    Stop,
    /// A session ended.
    Exit,
    /// An approval request was settled: by the approval policy, by default, or withdrawn.
    Approval,
    /// A caller answered a pending approval request through the supervisor.
    ApprovalAnswer,
}

impl AuditEvent {
    /// The stable name written to the log.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Refusal => "refusal",
            Self::Stop => "stop",
            Self::Exit => "exit",
            Self::Approval => "approval",
            Self::ApprovalAnswer => "approval_answer",
        }
    }
}

/// The limits a launch ran under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditLimits {
    /// The session's wall time.
    pub wall_time: Duration,
    /// The session's turn cap.
    pub max_turns: u32,
    /// The policy's depth limit.
    pub max_depth: u32,
    /// The policy's limit on live sessions.
    pub max_concurrent: u32,
    /// The policy's limit on one launcher's live sessions.
    pub max_children: u32,
}

/// An approval request a record is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditApproval {
    /// The crate's id for the request.
    pub id: String,
    /// What it would allow, as [`ApprovalKind`](crate::ApprovalKind) in lower case.
    pub kind: &'static str,
    /// Whether it was allowed.
    pub allowed: bool,
    /// Who settled it, as [`ApprovalSource`](crate::ApprovalSource) in lower case.
    pub source: &'static str,
}

/// One line of the audit log.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditRecord {
    /// When the decision was made.
    pub at: SystemTime,
    /// What the record is about.
    pub event: AuditEvent,
    /// Who asked: a principal, `operator`, or `supervisor` for a stop it made itself.
    pub caller: Option<String>,
    /// The caller's own depth: 0 for a configured launcher, the session's for a launched one.
    pub caller_depth: Option<u32>,
    /// The session the record is about.
    pub session: Option<u64>,
    /// The launched agent's principal.
    pub agent: Option<String>,
    /// The harness flavor.
    pub flavor: Option<&'static str>,
    /// The root the launch is confined to, as configured.
    pub root: Option<PathBuf>,
    /// The launched agent's depth.
    pub depth: Option<u32>,
    /// The limits the launch ran under.
    pub limits: Option<AuditLimits>,
    /// The flavor's known caveats.
    pub caveats: Vec<&'static str>,
    /// The approval the record is about.
    pub approval: Option<AuditApproval>,
    /// What came of it: a fixed code such as `accepted`, `depth_exceeded` or `wall_time_exceeded`.
    pub outcome: &'static str,
}

impl AuditRecord {
    /// A record of `event` with `outcome`, made now, and nothing else filled in.
    #[must_use]
    pub fn new(event: AuditEvent, outcome: &'static str) -> Self {
        Self {
            at: SystemTime::now(),
            event,
            caller: None,
            caller_depth: None,
            session: None,
            agent: None,
            flavor: None,
            root: None,
            depth: None,
            limits: None,
            caveats: Vec::new(),
            approval: None,
            outcome,
        }
    }

    /// The record as one JSON object, without a newline. Absent fields are left out.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        let millis = self.at.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let mut object = Map::new();
        object.insert("ts_ms".to_owned(), json!(u64::try_from(millis).unwrap_or(u64::MAX)));
        object.insert("event".to_owned(), json!(self.event.id()));
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                object.insert(key.to_owned(), value);
            }
        };
        put("caller", self.caller.as_ref().map(|caller| json!(caller)));
        put("caller_depth", self.caller_depth.map(|depth| json!(depth)));
        put("session", self.session.map(|session| json!(session)));
        put("agent", self.agent.as_ref().map(|agent| json!(agent)));
        put("flavor", self.flavor.map(|flavor| json!(flavor)));
        put("root", self.root.as_ref().map(|root| json!(root.to_string_lossy())));
        put("depth", self.depth.map(|depth| json!(depth)));
        put(
            "limits",
            self.limits.map(|limits| {
                json!({
                    "wall_time_s": limits.wall_time.as_secs(),
                    "max_turns": limits.max_turns,
                    "max_depth": limits.max_depth,
                    "max_concurrent": limits.max_concurrent,
                    "max_children": limits.max_children,
                })
            }),
        );
        put("caveats", (!self.caveats.is_empty()).then(|| json!(self.caveats)));
        put(
            "approval",
            self.approval.as_ref().map(|approval| {
                json!({
                    "id": approval.id,
                    "kind": approval.kind,
                    "allowed": approval.allowed,
                    "source": approval.source,
                })
            }),
        );
        object.insert("outcome".to_owned(), json!(self.outcome));
        Value::Object(object).to_string()
    }
}

/// Where audit records go. An implementation appends; it never rewrites or drops a record it
/// accepted.
pub trait AuditSink: Send + Sync {
    /// Appends `record`.
    ///
    /// # Errors
    ///
    /// The I/O error that kept the record from being stored. A supervisor refuses a launch
    /// whose launch record could not be stored, and counts the other failures.
    fn record(&self, record: &AuditRecord) -> std::io::Result<()>;
}

/// An audit log in a file: one JSON object per line, appended, the file readable and writable
/// by its owner only.
#[derive(Debug)]
pub struct FileAuditSink {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl FileAuditSink {
    /// Opens `path` for appending, creating it with mode 0600.
    ///
    /// # Errors
    ///
    /// The I/O error of opening it, and [`std::io::ErrorKind::PermissionDenied`] when `path` is
    /// a link, is not a regular file, belongs to another user, or is readable or writable by
    /// anyone but its owner: a log others can read leaks who launched what, and one they can
    /// write can be forged.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&path)?;
        check_private(&file)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// The log's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AuditSink for FileAuditSink {
    fn record(&self, record: &AuditRecord) -> std::io::Result<()> {
        let mut line = record.to_json_line();
        line.push('\n');
        let mut file = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // One write of the whole line: with `O_APPEND` concurrent writers do not interleave it.
        file.write_all(line.as_bytes())?;
        file.sync_data()
    }
}

/// Refuses a file that is not a regular file of this user with no group or other access.
fn check_private(file: &std::fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    let denied = |why: &str| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why.to_owned());
    if !metadata.is_file() {
        return Err(denied("the audit log is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(denied("the audit log belongs to another user"));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(denied("the audit log is accessible to users other than its owner"));
        }
    }
    Ok(())
}

/// An audit sink that keeps its records in memory, for tests and for an engine that forwards
/// them elsewhere.
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    records: Mutex<Vec<AuditRecord>>,
}

impl MemoryAuditSink {
    /// The records so far.
    #[must_use]
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, record: &AuditRecord) -> std::io::Result<()> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record.clone());
        Ok(())
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
