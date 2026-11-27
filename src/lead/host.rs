//! The lead's supervisor: built once at `serve --lead` start-up from the operator's file, bound to
//! this process, and called for the lead's own `agent_launch` / `agent_stop` calls.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use basemind_agents::{
    Caller, Event, FileAuditSink, HarnessKind, LaunchRequest, Peer, Registry, SessionId, SessionObserver, SessionState,
    Supervisor,
};
use serde_json::{Value, json};

use super::policy_file::{self, Context, Loaded};
use crate::comms::v2::{Actor, PostBody, PostTarget, Principal};
use crate::mcp::lead_tools::{self, AGENT_LAUNCH, AGENT_STOP};

/// Why this `serve --lead` hosts no supervisor. It then serves as a plain relay. The full reason
/// (`.0`, with paths and the policy file's own text) goes to stderr; the session's `instructions`,
/// which the model reads, get only the fixed category (`.1`).
#[derive(Clone, Debug)]
pub struct LeadOff(pub String, pub &'static str);

impl std::fmt::Display for LeadOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The fixed reasons a model may read for launching being off (review 3 N5).
const NO_POLICY: &str =
    "agent launching is off: there is no operator policy (agents.toml in basemind's user configuration directory)";
const DISABLED: &str = "agent launching is off: the operator policy agents.toml does not enable it";
const REFUSED: &str = "agent launching is off: the operator policy agents.toml was refused";
const NO_AUDIT: &str = "agent launching is off: the audit log cannot be used";
const NESTED: &str = "agent launching is off: this process runs inside a launched session";
const UNREADABLE: &str = "agent launching is off: this process cannot host a supervisor";

/// Where the host keeps what it needs besides the operator's file; injectable for tests.
pub struct Setup {
    /// The operator's policy file.
    pub policy_path: PathBuf,
    /// The directory the audit log is created in.
    pub audit_dir: PathBuf,
    /// The directory of the worktree lock files every lead of this user shares, so two leads
    /// never launch into the same or nested worktrees.
    pub worktree_locks: PathBuf,
    /// The adapters launches go through.
    pub registry: Registry,
}

impl Setup {
    /// The production setup: the policy file (the user-level `agents.toml`, or `policy_override`
    /// when the operator names one with `serve --agents-policy`) and `<data dir>/agents/`.
    pub fn from_user(policy_override: Option<&Path>) -> Result<Self, LeadOff> {
        let policy_path = policy_location(policy_override)?;
        let agents = crate::data_root::DataRoot::current().path().join("agents");
        Ok(Self {
            policy_path,
            worktree_locks: agents.join("worktrees"),
            audit_dir: agents,
            registry: Registry::with_defaults(),
        })
    }
}

/// The operator's policy file: `policy_override` (an absolute path the operator passed on the
/// command line, which the file checks of [`policy_file`] then apply to as to any other), else the
/// user-level `agents.toml`.
fn policy_location(policy_override: Option<&Path>) -> Result<PathBuf, LeadOff> {
    if let Some(path) = policy_override {
        if !path.is_absolute() {
            return Err(LeadOff(
                format!(
                    "agent launching is off: --agents-policy {} is not an absolute path",
                    path.display()
                ),
                "agent launching is off: the --agents-policy path is not absolute",
            ));
        }
        tracing::warn!(
            "agent launching reads its operator policy from {} (--agents-policy), not the user-level agents.toml",
            path.display()
        );
        return Ok(path.to_path_buf());
    }
    crate::config::agents_policy::policy_path().ok_or_else(|| {
        LeadOff(
            "agent launching is off: no home directory to find agents.toml in".to_owned(),
            "agent launching is off: there is no home directory to find the operator's agents.toml in",
        )
    })
}

/// The supervisor of one lead `serve`, and what it answers the lead's two tools with.
pub struct LeadHost {
    supervisor: Supervisor,
    caller: Caller,
    principal: Principal,
    lead_root: PathBuf,
    host: Option<String>,
    worktrees_root: PathBuf,
    /// Working directories of launches in progress (`None`) and of sessions, so two agents never
    /// share a worktree.
    worktrees: Mutex<Vec<(Option<SessionId>, PathBuf)>>,
    /// The lock files this lead holds for each session's worktree, shared with the observer,
    /// which drops them when the session finishes. A leaf lock: nothing else is taken under it.
    held_locks: Arc<HeldLocks>,
    worktree_locks: PathBuf,
    policy_path: PathBuf,
    audit_path: PathBuf,
}

impl LeadHost {
    /// Build the supervisor for the lead whose workspace is `root`, acting as `principal` (its
    /// comms identity, which the policy names as its only launcher), and bind it to this process.
    ///
    /// # Errors
    ///
    /// [`LeadOff`] naming why launching stays off: no policy file, a refused one, an audit log that
    /// cannot be opened or reserved, a process inside a launched session, or one that could not be
    /// sealed.
    pub fn start(root: &Path, principal: Principal, host: Option<String>, setup: Setup) -> Result<Self, LeadOff> {
        let lead_root = root.canonicalize().map_err(|error| {
            LeadOff(
                format!("agent launching is off: the workspace root: {error}"),
                "agent launching is off: the workspace root cannot be read",
            )
        })?;
        let off = |category: &'static str| {
            move |reason: String| LeadOff(format!("agent launching is off: {reason}"), category)
        };
        // Read once: after the binary is replaced on Linux it names `<path> (deleted)`, and every
        // later launch fails to resolve the injected server until the lead restarts.
        let mcp_command =
            std::env::current_exe().map_err(|error| off(UNREADABLE)(format!("this executable: {error}")))?;
        let env = |name: &str| std::env::var(name).ok();
        let context = Context {
            lead_root: &lead_root,
            launcher: principal.as_str(),
            mcp_command: &mcp_command,
            env: &env,
        };
        let path = &setup.policy_path;
        let lead = match policy_file::load(path, &context) {
            Ok(Loaded::Enabled(lead)) => lead,
            Ok(Loaded::Absent) => return Err(off(NO_POLICY)(format!("no operator policy at {}", path.display()))),
            Ok(Loaded::Disabled) => {
                return Err(off(DISABLED)(format!(
                    "{} does not enable [agents.launch]",
                    path.display()
                )));
            }
            Err(error) => return Err(off(REFUSED)(error.to_string())),
        };
        prune_audit_logs(&setup.audit_dir, AUDIT_RETENTION);
        let audit_path = open_audit_dir(&setup.audit_dir).map_err(|error| {
            off(NO_AUDIT)(format!(
                "the audit log directory {} cannot be used: {error}",
                setup.audit_dir.display()
            ))
        })?;
        let mut sink = FileAuditSink::open(&audit_path).map_err(|error| {
            off(NO_AUDIT)(format!(
                "the audit log {} cannot be opened: {error}",
                audit_path.display()
            ))
        })?;
        if let Some(max_bytes) = lead.audit_max_bytes {
            sink = sink.with_max_bytes(max_bytes);
        }
        let held_locks = Arc::new(HeldLocks::default());
        let supervisor = Supervisor::new(lead.policy, lead.configs, setup.registry, Arc::new(sink))
            .map_err(|error| {
                let category = match error {
                    basemind_agents::PolicyError::NestedHost => NESTED,
                    basemind_agents::PolicyError::AuditReserve { .. } => NO_AUDIT,
                    _ => REFUSED,
                };
                off(category)(error.to_string())
            })?
            .with_observer(Arc::new(LeadObserver {
                held_locks: Arc::clone(&held_locks),
            }));
        supervisor
            .register_launcher_in_process(principal.as_str())
            .map_err(|error| {
                let category = if error.code() == "nested_host" {
                    NESTED
                } else {
                    UNREADABLE
                };
                off(category)(format!("this process cannot host a supervisor ({})", error.code()))
            })?;
        let peer =
            Peer::embedding().map_err(|error| off(UNREADABLE)(format!("this process cannot be read: {error}")))?;
        let caller = Caller::principal(principal.as_str(), peer)
            .map_err(|error| off(REFUSED)(format!("the lead's principal: {error}")))?;
        Ok(Self {
            supervisor,
            caller,
            principal,
            lead_root,
            host,
            worktrees_root: lead.worktrees_root,
            worktrees: Mutex::default(),
            held_locks,
            worktree_locks: setup.worktree_locks,
            policy_path: setup.policy_path,
            audit_path,
        })
    }

    /// One line for the session's `instructions`: what the lead may do, and where it is configured.
    pub fn notice(&self) -> String {
        format!(
            "agent_launch / agent_stop are enabled by the operator policy {} (audit log {}).",
            self.policy_path.display(),
            self.audit_path.display()
        )
    }

    /// Stop every launched session and wait for it, its detached processes and its exit record
    /// ([`Supervisor::shutdown`]). Blocks; the `serve` calls it once its relay has ended, before
    /// the process exits.
    pub fn shutdown(&self) {
        self.supervisor.shutdown();
    }

    /// Answer a `tools/call` of `name`, one of the lead tools, as a `CallToolResult` value.
    pub async fn call(self: Arc<Self>, name: &str, arguments: Value) -> Value {
        let outcome = match name {
            AGENT_LAUNCH => self.launch(arguments).await,
            AGENT_STOP => self.stop(arguments).await,
            _ => Err(format!("unknown lead tool {name}")),
        };
        match outcome {
            Ok(body) => tool_result(&body, false),
            Err(message) => tool_result(&json!({ "error": message }), true),
        }
    }

    async fn launch(self: Arc<Self>, arguments: Value) -> Result<Value, String> {
        let params = lead_tools::parse_launch(arguments)?;
        let kind = harness_kind(&params.harness).ok_or("flavor_not_allowed")?;
        let (working_dir, locks) = self.claim_worktree(&params.working_dir)?;
        let relative = working_dir
            .strip_prefix(&self.worktrees_root)
            .map(Path::to_path_buf)
            .map_err(|_| "working_dir_invalid")?;
        let mut request = LaunchRequest::new(params.brief);
        request.working_dir = Some(relative);
        request.model = params.model;
        request.effort = params.effort;
        request.max_turns = params.max_turns;
        request.wall_time = params.wall_time_s.map(std::time::Duration::from_secs);
        let host = Arc::clone(&self);
        let launched = tokio::task::spawn_blocking(move || host.supervisor.launch(&host.caller, kind, &request))
            .await
            .map_err(|_| "launch_failed".to_owned())
            .and_then(|launched| launched.map_err(|error| error.code().to_owned()));
        let launched = match launched {
            Ok(launched) => launched,
            Err(code) => {
                self.release_worktree(&working_dir);
                return Err(code);
            }
        };
        self.bind_worktree(&working_dir, launched.id);
        self.held_locks.hold(launched.id, locks);
        if !self.live(launched.id) {
            // It finished before the observer could see its locks held.
            self.held_locks.release(launched.id);
        }
        let caveats: Vec<&str> = launched.caveats.iter().map(|caveat| caveat.id()).collect();
        let mut body = json!({
            "session": launched.id.get(),
            "agent": launched.agent,
            "depth": launched.depth,
            "caveats": caveats,
        });
        match self.open_thread(&launched.agent).await {
            Ok(thread) => body["thread"] = Value::String(thread),
            Err(reason) => {
                tracing::warn!(%reason, agent = %launched.agent, "agent_launch: no thread opened for the launched agent");
                body["thread"] = Value::Null;
                body["notice"] = Value::String(
                    "no basemind thread could be opened for the agent; post to it with agent_post".to_owned(),
                );
            }
        }
        Ok(body)
    }

    async fn stop(self: Arc<Self>, arguments: Value) -> Result<Value, String> {
        let params = lead_tools::parse_stop(arguments)?;
        let id = SessionId::from_raw(params.session);
        let host = Arc::clone(&self);
        tokio::task::spawn_blocking(move || host.supervisor.stop(&host.caller, id))
            .await
            .map_err(|_| "stop_failed".to_owned())?
            .map_err(|error| error.code().to_owned())?;
        Ok(json!({ "session": params.session, "stopping": true }))
    }

    /// Resolve `working_dir` under the worktrees root and reserve it. Refused, with a code that
    /// names no path, when it is not an existing directory strictly inside the root, or overlaps a
    /// directory another live session or launch holds, of this lead or of another lead of this
    /// user (through the shared lock files).
    fn claim_worktree(&self, working_dir: &str) -> Result<(PathBuf, WorktreeLocks), String> {
        let relative = Path::new(working_dir);
        let plain = relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        if working_dir.is_empty() || !plain {
            return Err("working_dir_invalid".to_owned());
        }
        let resolved = self
            .worktrees_root
            .join(relative)
            .canonicalize()
            .map_err(|_| "working_dir_invalid".to_owned())?;
        if !resolved.is_dir() || resolved == self.worktrees_root || !resolved.starts_with(&self.worktrees_root) {
            return Err("working_dir_invalid".to_owned());
        }
        if policy_file::overlaps(&resolved, &self.lead_root) {
            return Err("working_dir_invalid".to_owned());
        }
        let mut held = self.worktrees.lock().unwrap_or_else(PoisonError::into_inner);
        held.retain(|(session, _)| session.is_none_or(|id| self.live(id)));
        if held.iter().any(|(_, dir)| policy_file::overlaps(dir, &resolved)) {
            return Err("working_dir_in_use".to_owned());
        }
        let locks = lock_worktree(&self.worktree_locks, &resolved)?;
        held.push((None, resolved.clone()));
        Ok((resolved, locks))
    }

    fn bind_worktree(&self, dir: &Path, id: SessionId) {
        let mut held = self.worktrees.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = held.iter_mut().find(|(session, held)| session.is_none() && held == dir) {
            entry.0 = Some(id);
        }
    }

    fn release_worktree(&self, dir: &Path) {
        let mut held = self.worktrees.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = held.iter().position(|(session, held)| session.is_none() && held == dir) {
            held.remove(at);
        }
    }

    fn live(&self, id: SessionId) -> bool {
        self.supervisor
            .status(id)
            .is_some_and(|status| !matches!(status.state, SessionState::Finished(_)))
    }

    /// Open a thread from the lead to the launched agent, as the lead's comms principal.
    async fn open_thread(&self, agent: &str) -> Result<String, String> {
        let actor = Actor::parse(agent).map_err(|error| error.to_string())?;
        let hello =
            crate::comms::client::ClientHello::agent(self.principal.clone(), self.lead_root.clone(), self.host.clone());
        let mut client = crate::comms::client::CommsClient::ensure_and_connect(hello)
            .await
            .map_err(|error| error.to_string())?;
        let reply = client
            .agent_post(
                None,
                PostBody {
                    target: PostTarget::NewThread {
                        subject: format!("launched {agent}"),
                        to: vec![actor],
                        paths: Vec::new(),
                    },
                    body: format!(
                        "{} launched {agent}. Post progress and results to this thread.",
                        self.principal.as_str()
                    ),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(reply.thread.as_str().to_owned())
    }
}

/// The harness flavors the operator file can allow, by the name `agent_launch` takes.
fn harness_kind(name: &str) -> Option<HarnessKind> {
    match name {
        "claude" => Some(HarnessKind::ClaudeCode),
        "codex" => Some(HarnessKind::Codex),
        "opencode" => Some(HarnessKind::Acp(basemind_agents::AcpFlavor::OpenCode)),
        _ => None,
    }
}

/// A `CallToolResult` carrying `body` as JSON text.
fn tool_result(body: &Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": body.to_string() }],
        "isError": is_error,
    })
}

/// How long an audit log of an ended lead is kept (review 3 N1): each lead start adds a file of
/// up to 64 MiB.
const AUDIT_RETENTION: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Delete the audit logs in `dir` last written more than `retention` ago whose lead has ended: a
/// live lead's [`FileAuditSink`] holds its file's `flock`, so a file that cannot be locked is kept.
/// Best effort: a failure keeps the file.
fn prune_audit_logs(dir: &Path, retention: std::time::Duration) {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("launch-audit-") && name.ends_with(".jsonl")) {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > retention);
        if !old || !meta.is_file() {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(entry.path())
        else {
            continue;
        };
        // SAFETY: flock on a descriptor this function owns; it does not block (LOCK_NB).
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The audit log's path in `dir`, created mode 0700: one file per lead process.
fn open_audit_dir(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other("not a directory"));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    Ok(dir.join(format!("launch-audit-{}-{started}.jsonl", std::process::id())))
}

/// The worktree lock files of each live session.
#[derive(Default)]
struct HeldLocks(Mutex<HashMap<SessionId, WorktreeLocks>>);

impl HeldLocks {
    fn hold(&self, session: SessionId, locks: WorktreeLocks) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session, locks);
    }

    fn release(&self, session: SessionId) {
        let released = self.0.lock().unwrap_or_else(PoisonError::into_inner).remove(&session);
        drop(released);
    }
}

/// The worktree lock files this process has open, by path, with whether each is locked
/// exclusively. The locks are `fcntl` record locks, which belong to the process: two claims of one
/// process never conflict in the kernel, so this table is what refuses them, and closing any
/// descriptor of a file releases every lock the process holds on it, so a file is open at most
/// once per process: a second descriptor, closed by a refused claim or a finished session, would
/// release a lock another session of this process still relies on. Taken after a host's own locks.
static OPEN_LOCKS: Mutex<Vec<(PathBuf, bool, Weak<File>)>> = Mutex::new(Vec::new());

/// The lock files one worktree claim holds. They are closed under [`OPEN_LOCKS`], so no claim
/// opens a file again while its last descriptor is being closed.
struct WorktreeLocks(Vec<Arc<File>>);

impl Drop for WorktreeLocks {
    fn drop(&mut self) {
        let _open = OPEN_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
        self.0.clear();
    }
}

/// Take the lock files for `dir`, shared by every lead of this user that shares the data
/// directory: an exclusive lock on `dir`'s own file and a shared one on each of its ancestors up
/// to `/`, so a worktree is refused while another lead holds it, a directory inside it or one
/// around it, whatever worktrees root each lead has. The files are named by a hash of the
/// canonical path and are never deleted: deleting a lock file races with a lead opening it. The
/// locks go with the returned files.
///
/// Not `flock`: a `flock` lock belongs to the open file description, which every child this
/// process forks shares until it execs, so a lock released while any thread is spawning a process
/// (a launch of this lead, a `git`, another lead in the same process) outlives its release and
/// refuses the next claim of a worktree that is free. A record lock is never inherited by a fork.
fn lock_worktree(locks: &Path, dir: &Path) -> Result<WorktreeLocks, String> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(locks)
        .map_err(|_| "working_dir_lock_failed".to_owned())?;
    let mut open = OPEN_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    open.retain(|(_, _, file)| file.strong_count() > 0);
    // Declared after `open`, so a refused claim closes its files before the lock is let go.
    let mut held: Vec<Arc<File>> = Vec::new();
    let mut ancestors: Vec<&Path> = dir.ancestors().skip(1).collect();
    ancestors.reverse();
    for (path, exclusive) in ancestors.into_iter().map(|a| (a, false)).chain([(dir, true)]) {
        let lock_path = lock_file(locks, path);
        let known = open
            .iter()
            .find(|(known, _, _)| *known == lock_path)
            .and_then(|(_, held_exclusively, file)| file.upgrade().map(|file| (*held_exclusively, file)));
        match known {
            Some((false, file)) if !exclusive => {
                held.push(file);
                continue;
            }
            Some(_) => return Err("working_dir_in_use".to_owned()),
            None => {}
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(|_| "working_dir_lock_failed".to_owned())?;
        if !try_record_lock(&file, exclusive) {
            return Err("working_dir_in_use".to_owned());
        }
        let file = Arc::new(file);
        open.push((lock_path, exclusive, Arc::downgrade(&file)));
        held.push(file);
    }
    drop(open);
    Ok(WorktreeLocks(held))
}

/// The lock file in `locks` for the canonical directory `dir`.
fn lock_file(locks: &Path, dir: &Path) -> PathBuf {
    let name = blake3::hash(dir.as_os_str().as_encoded_bytes()).to_hex();
    locks.join(format!("{name}.lock"))
}

/// Lock the whole of `file` with a process-associated `fcntl` record lock, shared or exclusive,
/// without blocking. False when another process holds a conflicting lock.
fn try_record_lock(file: &File, exclusive: bool) -> bool {
    use std::os::unix::io::AsRawFd as _;
    // SAFETY: `flock` is plain integers, for which all zeroes is valid: start 0, length 0 (the
    // whole file).
    let mut range: libc::flock = unsafe { std::mem::zeroed() };
    range.l_type = (if exclusive { libc::F_WRLCK } else { libc::F_RDLCK }) as libc::c_short;
    range.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: F_SETLK on a descriptor the caller owns, reading the `flock` above; it does not block.
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &raw const range) == 0 }
}

/// Logs what the operator must see outside the audit log, and lets go of a finished session's
/// worktree locks. Never blocks.
struct LeadObserver {
    held_locks: Arc<HeldLocks>,
}

impl SessionObserver for LeadObserver {
    fn event(&self, session: SessionId, event: &Event) {
        match event {
            Event::Error(line) => tracing::warn!(%session, error = %line, "launched agent failed"),
            Event::Finished(status) => {
                tracing::info!(%session, ?status, "launched agent finished");
                self.held_locks.release(session);
            }
            _ => {}
        }
    }

    fn audit_exhausted(&self) {
        tracing::error!("the agent-launch audit log is exhausted: launching is off until it is rotated");
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
