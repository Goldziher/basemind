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
    /// The engine bound a launcher or the operator to a process.
    Registration,
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
            Self::Registration => "registration",
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
    /// Who asked, as the supervisor verified it: a principal, a launched agent's principal,
    /// `operator`, or `supervisor` for a stop it made itself. Absent when the caller could not
    /// be verified.
    pub caller: Option<String>,
    /// The principal an unverified caller claimed to be. Never a token.
    pub claimed: Option<String>,
    /// The caller's process id, as the kernel reported it.
    pub peer: Option<u32>,
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
    /// How many refusals like this one, of the same caller, were not written since the last
    /// one that was.
    pub suppressed: Option<u64>,
}

impl AuditRecord {
    /// A record of `event` with `outcome`, made now, and nothing else filled in.
    #[must_use]
    pub fn new(event: AuditEvent, outcome: &'static str) -> Self {
        Self {
            at: SystemTime::now(),
            event,
            caller: None,
            claimed: None,
            peer: None,
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
            suppressed: None,
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
        put("claimed", self.claimed.as_ref().map(|claimed| json!(claimed)));
        put("peer", self.peer.map(|peer| json!(peer)));
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
        if let Some(suppressed) = self.suppressed.filter(|suppressed| *suppressed > 0) {
            object.insert("suppressed".to_owned(), json!(suppressed));
        }
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

/// The size a [`FileAuditSink`] grows to by default before it drops records.
pub const DEFAULT_AUDIT_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Room past the cap kept for the markers that say records were dropped.
const MARKER_ROOM: u64 = 4096;

/// An audit log in a file: one JSON object per line, appended, the file readable and writable
/// by its owner only, and bounded in size.
///
/// The log is append-only as far as this writer goes; it is not tamper-resistant: any process
/// of the same user can rewrite or truncate it.
///
/// The last quarter of the cap ([`DEFAULT_AUDIT_MAX_BYTES`] unless
/// [`FileAuditSink::with_max_bytes`] says otherwise) is kept for launches, exits,
/// registrations, allows and the one stop that stops each session, so refusals, repeated stops
/// and denials, which a launched agent can loop on, cannot fill it. A record that does not fit is dropped and refused with an error: a supervisor then
/// refuses the launch or the caller's allow that record was for, stops a session whose
/// policy-settled allow it was, and counts the other failures. The first drop appends an
/// `audit_full` marker and dropping the sink an `audit_dropped` one with the count, in room
/// kept past the cap for them. Rotating the log is the operator's job.
#[derive(Debug)]
pub struct FileAuditSink {
    path: PathBuf,
    max_bytes: u64,
    file: Mutex<LogFile>,
}

#[derive(Debug)]
struct LogFile {
    file: std::fs::File,
    size: u64,
    dropped: u64,
}

impl FileAuditSink {
    /// Opens `path` for appending, creating it with mode 0600.
    ///
    /// # Errors
    ///
    /// The I/O error of opening it, and [`std::io::ErrorKind::PermissionDenied`] when `path` is
    /// a symbolic link, has another hard link, is not a regular file (a FIFO is refused without
    /// blocking), belongs to another user, or is readable or writable by anyone but its owner:
    /// a log others can read leaks who launched what, one they can write can be forged, and a
    /// hard link would have the records appended to whatever file it names.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Non-blocking, so a FIFO at the path cannot hold the open; cleared once the file
            // is known to be a regular file.
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        let file = options.open(&path)?;
        check_private(&file)?;
        #[cfg(unix)]
        blocking(&file)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            max_bytes: DEFAULT_AUDIT_MAX_BYTES,
            file: Mutex::new(LogFile { file, size, dropped: 0 }),
        })
    }

    /// The same sink, dropping records once the file would grow past `max_bytes`.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The log's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many records were dropped because the log was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.log().dropped
    }

    fn log(&self) -> std::sync::MutexGuard<'_, LogFile> {
        self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl LogFile {
    /// Appends `line` and syncs it. One write of the whole line: with `O_APPEND` concurrent
    /// writers do not interleave it.
    fn append(&mut self, line: &str) -> std::io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.size += line.len() as u64;
        self.file.sync_data()
    }

    /// A marker line of `event`, carrying `dropped` when given.
    fn marker(event: &str, dropped: Option<u64>) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut object = Map::new();
        object.insert("ts_ms".to_owned(), json!(u64::try_from(millis).unwrap_or(u64::MAX)));
        object.insert("event".to_owned(), json!(event));
        if let Some(dropped) = dropped {
            object.insert("dropped".to_owned(), json!(dropped));
        }
        object.insert("outcome".to_owned(), json!("dropped"));
        format!("{}\n", Value::Object(object))
    }
}

impl AuditSink for FileAuditSink {
    fn record(&self, record: &AuditRecord) -> std::io::Result<()> {
        let mut line = record.to_json_line();
        line.push('\n');
        let mut log = self.log();
        let limit = if kept_for(record) {
            self.max_bytes
        } else {
            self.max_bytes - self.max_bytes / 4
        };
        if log.size.saturating_add(line.len() as u64) > limit {
            log.dropped += 1;
            if log.dropped == 1 {
                let marker = LogFile::marker("audit_full", None);
                if log.size.saturating_add(marker.len() as u64) <= self.max_bytes.saturating_add(MARKER_ROOM) {
                    let _ = log.append(&marker);
                }
            }
            return Err(std::io::Error::other("the audit log is full"));
        }
        log.append(&line)
    }
}

/// Whether `record` may use the part of the cap kept for what must not go unrecorded: a
/// launch, an exit, a registration or its release, an allow, or a stop that stopped a session
/// (at most one per session). Never a refusal, a denial or a repeated stop, which a caller can
/// loop on.
fn kept_for(record: &AuditRecord) -> bool {
    match record.event {
        AuditEvent::Launch | AuditEvent::Exit => true,
        AuditEvent::Registration => matches!(record.outcome, "registered" | "unregistered"),
        AuditEvent::Approval | AuditEvent::ApprovalAnswer => {
            record.approval.as_ref().is_some_and(|approval| approval.allowed)
        }
        AuditEvent::Stop => matches!(record.outcome, "requested" | "cascaded"),
        AuditEvent::Refusal => false,
    }
}

impl Drop for FileAuditSink {
    /// Appends how many records were dropped, if any were.
    fn drop(&mut self) {
        let log = self.file.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.dropped > 0 {
            let marker = LogFile::marker("audit_dropped", Some(log.dropped));
            if log.size.saturating_add(marker.len() as u64) <= self.max_bytes.saturating_add(MARKER_ROOM) {
                let _ = log.append(&marker);
            }
        }
    }
}

/// Refuses a file that is not a regular file of this user with one link and no group or
/// other access.
fn check_private(file: &std::fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    let denied = |why: &str| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why.to_owned());
    if !metadata.is_file() {
        return Err(denied("the audit log is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(denied("the audit log has another hard link"));
        }
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

/// Clears `O_NONBLOCK`, set only so the open could not block.
#[cfg(unix)]
fn blocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a descriptor `file` owns, reading and then setting its status flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
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
