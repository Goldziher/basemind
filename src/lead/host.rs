//! The lead's supervisor: built once at `serve --lead` start-up from the operator's file, bound to
//! this process, and called for the lead's own `agent_launch` / `agent_stop` calls.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use basemind_agents::{
    Caller, Event, FileAuditSink, HarnessKind, LaunchRequest, Peer, Registry, SessionId, SessionObserver, SessionState,
    Supervisor,
};
use serde_json::{Value, json};

use super::policy_file::{self, Context, Loaded};
use crate::comms::v2::{Actor, PostBody, PostTarget, Principal};
use crate::mcp::lead_tools::{self, AGENT_LAUNCH, AGENT_STOP};

/// Why this `serve --lead` hosts no supervisor. It then serves as a plain relay, and the reason is
/// told to the human: on stderr and in the session's `instructions`.
#[derive(Clone, Debug)]
pub struct LeadOff(pub String);

impl std::fmt::Display for LeadOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

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
    /// The production setup: the user-level policy file and `<data dir>/agents/`.
    pub fn from_user() -> Result<Self, LeadOff> {
        let policy_path = crate::config::agents_policy::policy_path()
            .ok_or_else(|| LeadOff("agent launching is off: no home directory to find agents.toml in".to_owned()))?;
        let agents = crate::store::cache_root().join("agents");
        Ok(Self {
            policy_path,
            worktree_locks: agents.join("worktrees"),
            audit_dir: agents,
            registry: Registry::with_defaults(),
        })
    }
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
        let lead_root = root
            .canonicalize()
            .map_err(|error| LeadOff(format!("agent launching is off: the workspace root: {error}")))?;
        let off = |reason: String| LeadOff(format!("agent launching is off: {reason}"));
        let mcp_command = std::env::current_exe().map_err(|error| off(format!("this executable: {error}")))?;
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
            Ok(Loaded::Absent) => return Err(off(format!("no operator policy at {}", path.display()))),
            Ok(Loaded::Disabled) => return Err(off(format!("{} does not enable [agents.launch]", path.display()))),
            Err(error) => return Err(off(error.to_string())),
        };
        let audit_path = open_audit_dir(&setup.audit_dir).map_err(|error| {
            off(format!(
                "the audit log directory {} cannot be used: {error}",
                setup.audit_dir.display()
            ))
        })?;
        let mut sink = FileAuditSink::open(&audit_path).map_err(|error| {
            off(format!(
                "the audit log {} cannot be opened: {error}",
                audit_path.display()
            ))
        })?;
        if let Some(max_bytes) = lead.audit_max_bytes {
            sink = sink.with_max_bytes(max_bytes);
        }
        let held_locks = Arc::new(HeldLocks::default());
        let supervisor = Supervisor::new(lead.policy, lead.configs, setup.registry, Arc::new(sink))
            .map_err(|error| off(error.to_string()))?
            .with_observer(Arc::new(LeadObserver {
                held_locks: Arc::clone(&held_locks),
            }));
        supervisor
            .register_launcher_in_process(principal.as_str())
            .map_err(|error| off(format!("this process cannot host a supervisor ({})", error.code())))?;
        let peer = Peer::embedding().map_err(|error| off(format!("this process cannot be read: {error}")))?;
        let caller = Caller::principal(principal.as_str(), peer)
            .map_err(|error| off(format!("the lead's principal: {error}")))?;
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
    fn claim_worktree(&self, working_dir: &str) -> Result<(PathBuf, Vec<File>), String> {
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
        let locks = lock_worktree(&self.worktree_locks, &self.worktrees_root, &resolved)?;
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
struct HeldLocks(Mutex<HashMap<SessionId, Vec<File>>>);

impl HeldLocks {
    fn hold(&self, session: SessionId, locks: Vec<File>) {
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

/// Take the lock files for `dir` under `root`, shared by every lead of this user: an exclusive
/// lock on `dir`'s own file and a shared one on each directory between `root` and it, so a
/// worktree is refused while another lead holds it, a directory inside it or one around it.
/// The files are named by a hash of the canonical path and never deleted; the locks go with
/// the returned files.
fn lock_worktree(locks: &Path, root: &Path, dir: &Path) -> Result<Vec<File>, String> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    use std::os::unix::io::AsRawFd as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(locks)
        .map_err(|_| "working_dir_lock_failed".to_owned())?;
    let mut held = Vec::new();
    let mut ancestors: Vec<&Path> = dir.ancestors().skip(1).take_while(|a| a.starts_with(root)).collect();
    ancestors.reverse();
    for (path, exclusive) in ancestors.into_iter().map(|a| (a, false)).chain([(dir, true)]) {
        let name = blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(locks.join(format!("{name}.lock")))
            .map_err(|_| "working_dir_lock_failed".to_owned())?;
        let operation = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
        // SAFETY: flock on a descriptor this function owns; it does not block (LOCK_NB).
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
            return Err("working_dir_in_use".to_owned());
        }
        held.push(file);
    }
    Ok(held)
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
