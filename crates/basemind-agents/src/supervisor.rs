//! The [`Supervisor`]: the one place sessions are launched from, under a [`LaunchPolicy`].
//!
//! It decides who may launch which harness, at what depth and how many at once, writes an
//! audit record for every decision, owns the live sessions (status, stop, approvals, reaping)
//! and stops them all when it is dropped.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::audit::{AuditApproval, AuditEvent, AuditLimits, AuditRecord, AuditSink};
use crate::detached::{self, Detached};
use crate::error::LaunchError;
use crate::event::{Event, FinishStatus};
use crate::harness::HarnessKind;
use crate::identity::{AgentIdentity, LaunchToken};
use crate::policy::{Caveat, LAUNCHED_PREFIX, LaunchPolicy, PolicyError};
use crate::registry::Registry;
use crate::session::{Session, SessionHandle};
use crate::spec::{LaunchConfig, LaunchRequest};
use crate::supervisor_codes::{approval_kind, approval_source, finish_code};

pub use crate::supervisor_codes::launch_error_code;
use crate::supervisor_types::CallerKind;
pub use crate::supervisor_types::{
    Caller, Launched, SessionId, SessionObserver, SessionState, SessionStatus, SupervisorError,
};

/// The longest a dropped supervisor waits, past the sessions' stop grace, before it kills the
/// sessions still running and leaves their supervising threads to finish alone.
const DROP_SLACK: Duration = Duration::from_secs(2);
/// The most approval requests whose kind a session remembers until they are settled.
const MAX_REMEMBERED_APPROVALS: usize = 256;

/// Who launched a session.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Launcher {
    Principal(String),
    Session(u64),
}

/// The caller, once the supervisor has decided who it is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolved {
    Operator,
    Top(String),
    Session(u64),
}

struct Entry {
    flavor: HarnessKind,
    agent: String,
    launcher: Launcher,
    launcher_name: String,
    depth: u32,
    token: LaunchToken,
    state: SessionState,
    stop_requested: bool,
    started: SystemTime,
    root: PathBuf,
    handle: Option<SessionHandle>,
    detached: Arc<Detached>,
    approvals: HashMap<String, &'static str>,
    pump: Option<JoinHandle<()>>,
}

impl Entry {
    fn status(&self, id: u64) -> SessionStatus {
        SessionStatus {
            id: SessionId(id),
            flavor: self.flavor,
            agent: self.agent.clone(),
            launcher: self.launcher_name.clone(),
            depth: self.depth,
            state: self.state,
            started: self.started,
            pid: self.handle.as_ref().map(SessionHandle::pid),
            root: self.root.clone(),
            caveats: crate::policy::caveats(self.flavor),
        }
    }
}

#[derive(Default)]
struct Table {
    next: u64,
    launches: u32,
    sessions: BTreeMap<u64, Entry>,
}

impl Table {
    fn live(&self) -> impl Iterator<Item = (&u64, &Entry)> {
        self.sessions.iter().filter(|(_, entry)| entry.state.live())
    }
}

struct Inner {
    policy: LaunchPolicy,
    configs: Vec<LaunchConfig>,
    registry: Registry,
    audit: Arc<dyn AuditSink>,
    observer: std::sync::OnceLock<Arc<dyn SessionObserver>>,
    table: Mutex<Table>,
    changed: Condvar,
    audit_failures: AtomicU64,
}

impl Inner {
    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Appends `record`, counting a failure.
    fn audit(&self, record: &AuditRecord) -> std::io::Result<()> {
        let written = self.audit.record(record);
        if written.is_err() {
            self.audit_failures.fetch_add(1, Ordering::SeqCst);
        }
        written
    }

    fn limits(&self, config: &LaunchConfig) -> AuditLimits {
        AuditLimits {
            wall_time: config.limits.wall_time,
            max_turns: config.limits.max_turns,
            max_depth: self.policy.max_depth(),
            max_concurrent: self.policy.max_concurrent(),
            max_children: self.policy.max_children(),
        }
    }
}

/// Launches sessions under a [`LaunchPolicy`] and owns them until they are reaped.
///
/// Dropping the supervisor stops every live session, waits for them within their stop grace,
/// kills what is left, and returns.
pub struct Supervisor {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("policy", &self.inner.policy)
            .field("sessions", &self.inner.table().sessions.len())
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    /// A supervisor enforcing `policy` over launches of `configs` (one per allowed flavor)
    /// through `registry`, writing to `audit`.
    ///
    /// # Errors
    ///
    /// [`PolicyError::ConfigNotAllowed`] for a config of a flavor the policy does not allow
    /// (with the default policy, any config), [`PolicyError::DuplicateConfig`] for two of one
    /// flavor, and [`PolicyError::ConfigMismatch`] for a config that widens what the policy's
    /// rule for its flavor grants: another permission mode, bypass or unverified-mode opt-in, an
    /// approval policy the rule does not consent to, a pre-approved tool the rule does not
    /// name, or a stop grace over [`MAX_STOP_GRACE_CEILING`](crate::MAX_STOP_GRACE_CEILING).
    pub fn new(
        policy: LaunchPolicy,
        configs: Vec<LaunchConfig>,
        registry: Registry,
        audit: Arc<dyn AuditSink>,
    ) -> Result<Self, PolicyError> {
        for (at, config) in configs.iter().enumerate() {
            let kind = config.harness.kind;
            let Some(rule) = policy.flavor(kind) else {
                return Err(PolicyError::ConfigNotAllowed(kind.id()));
            };
            if configs[..at].iter().any(|earlier| earlier.harness.kind == kind) {
                return Err(PolicyError::DuplicateConfig(kind.id()));
            }
            crate::policy::check_config(rule, config)?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                policy,
                configs,
                registry,
                audit,
                observer: std::sync::OnceLock::new(),
                table: Mutex::default(),
                changed: Condvar::new(),
                audit_failures: AtomicU64::new(0),
            }),
        })
    }

    /// The same supervisor reporting every session event to `observer`. Only the first observer
    /// set is kept; a session sees it from its next event on.
    #[must_use]
    pub fn with_observer(self, observer: Arc<dyn SessionObserver>) -> Self {
        let _ = self.inner.observer.set(observer);
        self
    }

    /// The policy in force.
    #[must_use]
    pub fn policy(&self) -> &LaunchPolicy {
        &self.inner.policy
    }

    /// How many audit records could not be written. A failed launch record refuses the launch;
    /// the others (stop, exit, approval) are counted here.
    #[must_use]
    pub fn audit_failures(&self) -> u64 {
        self.inner.audit_failures.load(Ordering::SeqCst)
    }

    /// Launches `request` in a harness of `flavor` for `caller`.
    ///
    /// The launched agent gets a principal of its own (`launched-<flavor>-<id>-<hex>`), its depth
    /// in `BASEMIND_AGENT_DEPTH` and a fresh token in `BASEMIND_LAUNCH_TOKEN`. Its wall time is
    /// the least of the request's, the config's and the policy's; its turns are capped by the
    /// budget too. A launch record is written before the harness starts; a refusal record for
    /// every refusal, the adapter's included.
    ///
    /// # Errors
    ///
    /// [`SupervisorError::Disabled`], [`SupervisorError::CallerNotAdmitted`] (the operator, an
    /// unknown principal, a finished session), [`SupervisorError::UnknownToken`],
    /// [`SupervisorError::FlavorNotAllowed`], [`SupervisorError::DepthExceeded`],
    /// [`SupervisorError::BudgetExhausted`], [`SupervisorError::ConcurrencyExceeded`],
    /// [`SupervisorError::FanOutExceeded`], [`SupervisorError::Token`],
    /// [`SupervisorError::Audit`], and the adapter's errors as [`SupervisorError::Launch`].
    pub fn launch(
        &self,
        caller: &Caller,
        flavor: HarnessKind,
        request: &LaunchRequest,
    ) -> Result<Launched, SupervisorError> {
        let inner = &self.inner;
        let mut refusal = AuditRecord {
            flavor: Some(flavor.id()),
            ..AuditRecord::new(AuditEvent::Refusal, "")
        };
        let mut table = inner.table();
        let admitted = admit(inner, &table, caller, flavor, &mut refusal);
        let (launcher, launcher_name, depth, config) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                refusal.outcome = error.code();
                let _ = inner.audit(&refusal);
                return Err(error);
            }
        };
        let token = match LaunchToken::generate() {
            Ok(token) => token,
            Err(error) => return Err(refuse(inner, refusal, SupervisorError::Token(error))),
        };
        let id = table.next;
        let agent = match crate::identity::random_bytes::<4>() {
            Ok(bytes) => format!("{LAUNCHED_PREFIX}{}-{id}-{}", flavor.id(), crate::identity::hex(&bytes)),
            Err(error) => return Err(refuse(inner, refusal, SupervisorError::Token(error))),
        };
        let config = match launched_config(inner, config, &agent, depth, token.clone()) {
            Ok(config) => config,
            Err(error) => return Err(refuse(inner, refusal, error.into())),
        };
        let caveats = crate::policy::caveats(flavor);
        let record = AuditRecord {
            session: Some(id),
            agent: Some(agent.clone()),
            root: Some(config.root.clone()),
            depth: Some(depth),
            limits: Some(inner.limits(&config)),
            caveats: caveats.iter().map(|caveat| caveat.id()).collect(),
            ..AuditRecord {
                event: AuditEvent::Launch,
                outcome: "accepted",
                ..refusal.clone()
            }
        };
        if let Err(error) = inner.audit(&record) {
            return Err(refuse(inner, refusal, SupervisorError::Audit(error)));
        }
        table.next += 1;
        table.launches += 1;
        let root = config.root.clone();
        table.sessions.insert(
            id,
            Entry {
                flavor,
                agent: agent.clone(),
                launcher,
                launcher_name,
                depth,
                token,
                state: SessionState::Starting,
                stop_requested: false,
                started: SystemTime::now(),
                root,
                handle: None,
                detached: Arc::default(),
                approvals: HashMap::new(),
                pump: None,
            },
        );
        drop(table);

        let session = match inner.registry.launch(&config, request) {
            Ok(session) => session,
            Err(error) => {
                let mut table = inner.table();
                table.sessions.remove(&id);
                table.launches -= 1;
                drop(table);
                inner.changed.notify_all();
                let error = SupervisorError::Launch(error);
                let record = AuditRecord {
                    session: Some(id),
                    outcome: error.code(),
                    ..refusal
                };
                let _ = inner.audit(&record);
                return Err(error);
            }
        };
        let handle = session.handle();
        let mut table = inner.table();
        let entry = table
            .sessions
            .get_mut(&id)
            .expect("only the pump removes a started entry");
        entry.handle = Some(handle.clone());
        if entry.stop_requested {
            entry.state = SessionState::Stopping;
            handle.stop();
        } else {
            entry.state = SessionState::Running;
        }
        let detached = entry.detached.clone();
        entry.pump = Some(std::thread::spawn({
            let inner = self.inner.clone();
            move || supervise(&inner, id, session, &detached, flavor)
        }));
        drop(table);
        Ok(Launched {
            id: SessionId(id),
            agent,
            depth,
            caveats,
        })
    }

    /// A snapshot of session `id`, until it is reaped.
    #[must_use]
    pub fn status(&self, id: SessionId) -> Option<SessionStatus> {
        self.inner.table().sessions.get(&id.0).map(|entry| entry.status(id.0))
    }

    /// Snapshots of every session not yet reaped, in launch order.
    #[must_use]
    pub fn list(&self) -> Vec<SessionStatus> {
        let table = self.inner.table();
        table.sessions.iter().map(|(id, entry)| entry.status(*id)).collect()
    }

    /// Waits up to `timeout` for session `id` to finish, and returns its state then.
    #[must_use]
    pub fn wait(&self, id: SessionId, timeout: Duration) -> Option<SessionState> {
        let deadline = Instant::now() + timeout;
        let mut table = self.inner.table();
        loop {
            let state = table.sessions.get(&id.0)?.state;
            let now = Instant::now();
            if !state.live() || now >= deadline {
                return Some(state);
            }
            table = self
                .inner
                .changed
                .wait_timeout(table, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Asks session `id` to stop, within its stop grace. A finished session is left as it is.
    ///
    /// # Errors
    ///
    /// [`SupervisorError::UnknownSession`], and [`SupervisorError::NotOwner`] unless `caller` is
    /// the operator or launched the session.
    pub fn stop(&self, caller: &Caller, id: SessionId) -> Result<(), SupervisorError> {
        let inner = &self.inner;
        let mut table = inner.table();
        let resolved = resolve(&inner.policy, &table, caller);
        let mut record = AuditRecord {
            session: Some(id.0),
            caller: resolved.as_ref().ok().map(|resolved| name_of(&table, resolved)),
            ..AuditRecord::new(AuditEvent::Stop, "requested")
        };
        let entry = match owned(&mut table, resolved, id) {
            Ok(entry) => entry,
            Err(error) => {
                record.outcome = error.code();
                drop(table);
                let _ = inner.audit(&record);
                return Err(error);
            }
        };
        record.flavor = Some(entry.flavor.id());
        record.agent = Some(entry.agent.clone());
        if entry.state.live() {
            request_stop(entry);
        } else {
            record.outcome = "already_finished";
        }
        drop(table);
        let _ = inner.audit(&record);
        Ok(())
    }

    /// Stops every live session, as the supervisor itself.
    pub fn stop_all(&self) {
        let inner = &self.inner;
        let mut stopped = Vec::new();
        let mut table = inner.table();
        for (id, entry) in &mut table.sessions {
            if entry.state.live() {
                request_stop(entry);
                stopped.push((*id, entry.flavor, entry.agent.clone()));
            }
        }
        drop(table);
        for (id, flavor, agent) in stopped {
            let _ = inner.audit(&AuditRecord {
                caller: Some("supervisor".to_owned()),
                session: Some(id),
                flavor: Some(flavor.id()),
                agent: Some(agent),
                ..AuditRecord::new(AuditEvent::Stop, "requested")
            });
        }
    }

    /// Allows (`allow`) or denies the pending approval request `approval` of session `id`.
    ///
    /// # Errors
    ///
    /// [`SupervisorError::UnknownSession`], [`SupervisorError::NotOwner`] unless `caller` is
    /// the operator or launched the session, and the session's own errors (a finished session,
    /// a full control queue) as [`SupervisorError::Launch`].
    pub fn answer(&self, caller: &Caller, id: SessionId, approval: &str, allow: bool) -> Result<(), SupervisorError> {
        let inner = &self.inner;
        let mut table = inner.table();
        let resolved = resolve(&inner.policy, &table, caller);
        let caller_name = resolved.as_ref().ok().map(|resolved| name_of(&table, resolved));
        let entry = match owned(&mut table, resolved, id) {
            Ok(entry) => entry,
            Err(error) => {
                drop(table);
                let _ = inner.audit(&AuditRecord {
                    caller: caller_name,
                    session: Some(id.0),
                    ..AuditRecord::new(AuditEvent::ApprovalAnswer, error.code())
                });
                return Err(error);
            }
        };
        // An id the session did not issue is the caller's own text: it is not recorded.
        let (recorded_id, kind) = match entry.approvals.get(approval) {
            Some(kind) => (approval.to_owned(), *kind),
            None => ("unknown".to_owned(), "unknown"),
        };
        let handle = entry.handle.clone();
        let flavor = entry.flavor.id();
        drop(table);
        let answered = handle
            .ok_or(LaunchError::SessionClosed)
            .and_then(|handle| handle.answer(approval, allow))
            .map_err(SupervisorError::Launch);
        let outcome = match (&answered, allow) {
            (Err(error), _) => error.code(),
            (Ok(()), true) => "allowed",
            (Ok(()), false) => "denied",
        };
        let _ = inner.audit(&AuditRecord {
            caller: caller_name,
            session: Some(id.0),
            flavor: Some(flavor),
            approval: Some(AuditApproval {
                id: recorded_id,
                kind,
                allowed: allow && answered.is_ok(),
                source: "caller",
            }),
            ..AuditRecord::new(AuditEvent::ApprovalAnswer, outcome)
        });
        answered
    }

    /// Removes the finished sessions and returns their last snapshots.
    pub fn reap(&self) -> Vec<SessionStatus> {
        let mut table = self.inner.table();
        let finished: Vec<u64> = table
            .sessions
            .iter()
            .filter(|(_, entry)| !entry.state.live())
            .map(|(id, _)| *id)
            .collect();
        let mut reaped = Vec::new();
        let mut pumps = Vec::new();
        for id in finished {
            if let Some(mut entry) = table.sessions.remove(&id) {
                reaped.push(entry.status(id));
                pumps.extend(entry.pump.take());
            }
        }
        drop(table);
        for pump in pumps {
            let _ = pump.join();
        }
        reaped
    }
}

impl Drop for Supervisor {
    /// Stops every live session, waits for them within their stop grace and the detached-group
    /// grace, then kills the ones still running and waits briefly once more.
    fn drop(&mut self) {
        self.stop_all();
        let grace = self
            .inner
            .configs
            .iter()
            .map(|config| config.limits.stop_grace)
            .max()
            .unwrap_or_default();
        if !self.wait_all(grace + detached::EXIT_GRACE + DROP_SLACK) {
            for entry in self.inner.table().sessions.values() {
                if let Some(handle) = entry.handle.as_ref().filter(|_| entry.state.live()) {
                    handle.kill();
                }
            }
            let _ = self.wait_all(detached::EXIT_GRACE + DROP_SLACK);
        }
        let pumps: Vec<JoinHandle<()>> = {
            let mut table = self.inner.table();
            table
                .sessions
                .values_mut()
                .filter(|entry| !entry.state.live())
                .filter_map(|entry| entry.pump.take())
                .collect()
        };
        for pump in pumps {
            let _ = pump.join();
        }
    }
}

impl Supervisor {
    /// Waits up to `timeout` for every session to finish; whether they all did.
    fn wait_all(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut table = self.inner.table();
        loop {
            if table.live().next().is_none() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            table = self
                .inner
                .changed
                .wait_timeout(table, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// Records `error` as the refusal's outcome, writes it, and returns the error.
fn refuse(inner: &Inner, mut refusal: AuditRecord, error: SupervisorError) -> SupervisorError {
    refusal.outcome = error.code();
    let _ = inner.audit(&refusal);
    error
}

/// Every check a launch passes before anything is started: the policy is on, the caller is
/// admitted, the flavor is allowed and configured, and depth, budget, concurrency and fan-out
/// are within their limits. Fills in what the refusal record learns on the way.
fn admit<'a>(
    inner: &'a Inner,
    table: &Table,
    caller: &Caller,
    flavor: HarnessKind,
    refusal: &mut AuditRecord,
) -> Result<(Launcher, String, u32, &'a LaunchConfig), SupervisorError> {
    let policy = &inner.policy;
    if !policy.enabled() {
        return Err(SupervisorError::Disabled);
    }
    // Until the caller resolves, a refusal names whom it claimed to be; never a token.
    refusal.caller = match &caller.kind {
        CallerKind::Operator => Some("operator".to_owned()),
        CallerKind::Principal(principal) => Some(principal.clone()),
        CallerKind::Token(_) => None,
    };
    let resolved = resolve(policy, table, caller)?;
    refusal.caller = Some(name_of(table, &resolved));
    let (launcher, caller_depth) = match resolved {
        Resolved::Operator => return Err(SupervisorError::CallerNotAdmitted),
        Resolved::Top(principal) => (Launcher::Principal(principal), 0),
        Resolved::Session(id) => (Launcher::Session(id), table.sessions[&id].depth),
    };
    refusal.caller_depth = Some(caller_depth);
    let launcher_name = refusal.caller.clone().unwrap_or_default();
    let config = policy
        .flavor(flavor)
        .and_then(|_| inner.configs.iter().find(|config| config.harness.kind == flavor))
        .ok_or(SupervisorError::FlavorNotAllowed(flavor.id()))?;
    let depth = caller_depth + 1;
    refusal.depth = Some(depth);
    if depth > policy.max_depth() {
        return Err(SupervisorError::DepthExceeded {
            depth,
            max_depth: policy.max_depth(),
        });
    }
    if let Some(budget) = policy.budget().filter(|budget| table.launches >= budget.max_launches) {
        return Err(SupervisorError::BudgetExhausted {
            max_launches: budget.max_launches,
        });
    }
    let live = table.live().count();
    if live >= usize::try_from(policy.max_concurrent()).unwrap_or(usize::MAX) {
        return Err(SupervisorError::ConcurrencyExceeded {
            max_concurrent: policy.max_concurrent(),
        });
    }
    let children = table.live().filter(|(_, entry)| entry.launcher == launcher).count();
    if children >= usize::try_from(policy.max_children()).unwrap_or(usize::MAX) {
        return Err(SupervisorError::FanOutExceeded {
            max_children: policy.max_children(),
        });
    }
    Ok((launcher, launcher_name, depth, config))
}

/// The flavor's config as the launched agent runs it: its own principal (keeping the config's
/// host), its depth and token, and limits lowered to the policy's wall time and budget.
fn launched_config(
    inner: &Inner,
    template: &LaunchConfig,
    agent: &str,
    depth: u32,
    token: LaunchToken,
) -> Result<LaunchConfig, LaunchError> {
    let mut identity = AgentIdentity::new(agent)?;
    if let Some(host) = template.identity.host() {
        identity = identity.with_host(host)?;
    }
    let mut config = template.clone();
    config.identity = identity.with_lineage(depth, token);
    config.limits.wall_time = config.limits.wall_time.min(inner.policy.wall_time());
    if let Some(budget) = inner.policy.budget() {
        config.limits.max_turns = config.limits.max_turns.min(budget.max_turns);
    }
    Ok(config)
}

/// Who `caller` is. A peer process inside a live session's process group, or in a group the
/// session's harness moved its descendants into, is that session whatever it claims; a token
/// must belong to a live session; a principal must be one the policy admits.
fn resolve(policy: &LaunchPolicy, table: &Table, caller: &Caller) -> Result<Resolved, SupervisorError> {
    if let Some(group) = caller.peer_pid.and_then(detached::group_of) {
        let owner = table.live().find(|(_, entry)| {
            entry.handle.as_ref().is_some_and(|handle| handle.pid() == group)
                || entry
                    .detached
                    .group_ids()
                    .iter()
                    .any(|pgid| u32::try_from(*pgid).ok() == Some(group))
        });
        if let Some((id, _)) = owner {
            return Ok(Resolved::Session(*id));
        }
    }
    match &caller.kind {
        CallerKind::Operator => Ok(Resolved::Operator),
        CallerKind::Token(token) => table
            .live()
            .find(|(_, entry)| entry.token.matches(token))
            .map(|(id, _)| Resolved::Session(*id))
            .ok_or(SupervisorError::UnknownToken),
        CallerKind::Principal(principal) if policy.admits(principal) => Ok(Resolved::Top(principal.clone())),
        CallerKind::Principal(_) => Err(SupervisorError::CallerNotAdmitted),
    }
}

/// The name a resolved caller is recorded under.
fn name_of(table: &Table, resolved: &Resolved) -> String {
    match resolved {
        Resolved::Operator => "operator".to_owned(),
        Resolved::Top(principal) => principal.clone(),
        Resolved::Session(id) => table
            .sessions
            .get(id)
            .map_or_else(|| format!("session-{id}"), |entry| entry.agent.clone()),
    }
}

/// Session `id`, when `resolved` may control it: the operator any, a launcher those it launched.
fn owned(
    table: &mut Table,
    resolved: Result<Resolved, SupervisorError>,
    id: SessionId,
) -> Result<&mut Entry, SupervisorError> {
    let resolved = resolved?;
    let entry = table
        .sessions
        .get_mut(&id.0)
        .ok_or(SupervisorError::UnknownSession(id))?;
    let allowed = match (&resolved, &entry.launcher) {
        (Resolved::Operator, _) => true,
        (Resolved::Top(principal), Launcher::Principal(launcher)) => principal == launcher,
        (Resolved::Session(caller), Launcher::Session(launcher)) => caller == launcher,
        _ => false,
    };
    if allowed {
        Ok(entry)
    } else {
        Err(SupervisorError::NotOwner(id))
    }
}

fn request_stop(entry: &mut Entry) {
    match &entry.handle {
        Some(handle) => {
            entry.state = SessionState::Stopping;
            handle.stop();
        }
        // Still starting: the launch stops it as soon as it has the handle.
        None => entry.stop_requested = true,
    }
}

/// The thread that owns one session: it reads every event, hands it to the observer, audits
/// each settled approval, records the harness's detached groups for a flavor that makes them,
/// and once the session has finished, reaps those groups, marks the session finished and
/// writes its exit record.
fn supervise(inner: &Inner, id: u64, session: Session, detached: &Arc<Detached>, flavor: HarnessKind) {
    let tracks = crate::policy::caveats(flavor).contains(&Caveat::DetachedServer);
    let done = Arc::new(AtomicBool::new(false));
    let tracker = tracks.then(|| {
        let (detached, done, pid) = (detached.clone(), done.clone(), session.pid());
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                detached.poll(pid);
                std::thread::sleep(detached::POLL);
            }
        })
    });
    let mut status = FinishStatus::Failed;
    for event in session.events() {
        if let Some(observer) = inner.observer.get() {
            observer.event(SessionId(id), &event);
        }
        match &event {
            Event::ApprovalRequest(request) => {
                let mut table = inner.table();
                if let Some(entry) = table.sessions.get_mut(&id)
                    && entry.approvals.len() < MAX_REMEMBERED_APPROVALS
                {
                    entry.approvals.insert(request.id.clone(), approval_kind(request.kind));
                }
            }
            Event::ApprovalResolved {
                id: approval,
                allowed,
                source,
            } => {
                let kind = inner
                    .table()
                    .sessions
                    .get_mut(&id)
                    .and_then(|entry| entry.approvals.remove(approval))
                    .unwrap_or("unknown");
                let _ = inner.audit(&AuditRecord {
                    session: Some(id),
                    flavor: Some(flavor.id()),
                    approval: Some(AuditApproval {
                        id: approval.clone(),
                        kind,
                        allowed: *allowed,
                        source: approval_source(*source),
                    }),
                    ..AuditRecord::new(AuditEvent::Approval, if *allowed { "allowed" } else { "denied" })
                });
            }
            Event::Finished(finished) => status = *finished,
            _ => {}
        }
    }
    done.store(true, Ordering::SeqCst);
    if let Some(tracker) = tracker {
        let _ = tracker.join();
    }
    let killed = if tracks { detached.reap(detached::EXIT_GRACE) } else { 0 };
    drop(session);
    let mut table = inner.table();
    let record = table.sessions.get_mut(&id).map(|entry| {
        entry.state = SessionState::Finished(status);
        entry.handle = None;
        AuditRecord {
            session: Some(id),
            agent: Some(entry.agent.clone()),
            flavor: Some(flavor.id()),
            depth: Some(entry.depth),
            caveats: if killed > 0 {
                vec![Caveat::DetachedServer.id()]
            } else {
                Vec::new()
            },
            ..AuditRecord::new(AuditEvent::Exit, finish_code(status))
        }
    });
    drop(table);
    inner.changed.notify_all();
    if let Some(record) = record {
        let _ = inner.audit(&record);
    }
}

#[cfg(all(test, unix))]
#[path = "supervisor_tests.rs"]
mod tests;
