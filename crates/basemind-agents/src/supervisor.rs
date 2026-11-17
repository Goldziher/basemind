//! The [`Supervisor`]: the one place sessions are launched from, under a [`LaunchPolicy`].
//!
//! It decides who may launch which harness, at what depth and how many at once, writes an
//! audit record for every decision, owns the live sessions (status, stop, approvals, reaping)
//! and stops them all when it is dropped.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::audit::{AuditApproval, AuditEvent, AuditLimits, AuditRecord, AuditSink};
use crate::detached::{self, Detached};
use crate::error::LaunchError;
use crate::harness::HarnessKind;
use crate::identity::LaunchToken;
use crate::policy::{LAUNCHED_PREFIX, LaunchPolicy, PolicyError};
use crate::procinfo::{self, ProcId};
use crate::refusals::Refusals;
use crate::registry::Registry;
use crate::session::SessionHandle;
use crate::spec::{LaunchConfig, LaunchRequest};
use crate::supervisor_admit::{Admitted, Asker, admit, launched_config, owned, refuse};
use crate::supervisor_allows::Allowance;
use crate::supervisor_pump::{Reservation, supervise};

pub use crate::supervisor_codes::launch_error_code;
use crate::supervisor_identity::{Role, resolve};
pub use crate::supervisor_types::{
    Caller, Launched, SessionId, SessionObserver, SessionState, SessionStatus, SupervisorError,
};

/// The longest a dropped supervisor waits, past the sessions' stop grace, before it kills the
/// sessions still running and leaves their supervising threads to finish alone.
const DROP_SLACK: Duration = Duration::from_secs(2);

/// Who launched a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Launcher {
    Principal(String),
    Session(u64),
}

/// The caller, once the supervisor has decided who it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    Operator,
    Top(String),
    Session(u64),
}

pub(crate) struct Entry {
    pub(crate) flavor: HarnessKind,
    pub(crate) agent: String,
    pub(crate) launcher: Launcher,
    /// The sessions above this one, outermost first: a stop of any of them stops this one.
    pub(crate) lineage: Vec<u64>,
    launcher_name: String,
    pub(crate) depth: u32,
    pub(crate) token: LaunchToken,
    /// The harness's pid and start time, once it runs.
    pub(crate) leader: Option<ProcId>,
    pub(crate) state: SessionState,
    stop_requested: bool,
    started: SystemTime,
    root: PathBuf,
    pub(crate) handle: Option<SessionHandle>,
    pub(crate) detached: Arc<Detached>,
    pub(crate) approvals: HashMap<String, &'static str>,
    /// What the session may still be allowed.
    allowance: Allowance,
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
pub(crate) struct Table {
    next: u64,
    pub(crate) launches: u32,
    pub(crate) sessions: BTreeMap<u64, Entry>,
    /// The processes the engine registered, each with what it may act as.
    pub(crate) registered: Vec<(Role, ProcId)>,
}

impl Table {
    pub(crate) fn live(&self) -> impl Iterator<Item = (&u64, &Entry)> {
        self.sessions.iter().filter(|(_, entry)| entry.state.live())
    }
}

pub(crate) struct Inner {
    pub(crate) policy: LaunchPolicy,
    pub(crate) configs: Vec<LaunchConfig>,
    registry: Registry,
    audit: Arc<dyn AuditSink>,
    pub(crate) observer: std::sync::OnceLock<Arc<dyn SessionObserver>>,
    table: Mutex<Table>,
    pub(crate) changed: Condvar,
    pub(crate) audit_failures: AtomicU64,
    /// The registrations and releases recorded, against
    /// [`MAX_REGISTRATIONS`](crate::supervisor_reserve::MAX_REGISTRATIONS).
    pub(crate) registrations: AtomicU32,
    /// The allows given over the supervisor's life, against the budget's `max_allows`.
    allows: Arc<AtomicU32>,
    /// Set once a record the reserve holds room for could not be written.
    pub(crate) exhausted: Arc<AtomicBool>,
    refusals: Refusals,
    /// Serializes registrations, each with its record.
    pub(crate) registering: Mutex<()>,
}

impl Inner {
    pub(crate) fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits on `table` for a change until `deadline` (forever without one). `None` once the
    /// deadline has passed.
    fn until<'a>(&self, table: MutexGuard<'a, Table>, deadline: Option<Instant>) -> Option<MutexGuard<'a, Table>> {
        Some(match deadline {
            Some(deadline) => {
                let left = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())?;
                self.changed
                    .wait_timeout(table, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            }
            None => self
                .changed
                .wait(table)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        })
    }

    /// Appends `record`, counting a failure. A record the reserve was claimed for that still
    /// cannot be written exhausts the log: from then on the supervisor launches and allows
    /// nothing, and tells the observer once.
    pub(crate) fn audit(&self, record: &AuditRecord) -> std::io::Result<()> {
        let written = self.audit.record(record);
        if written.is_err() {
            self.audit_failures.fetch_add(1, Ordering::SeqCst);
            if crate::audit::kept_for(record)
                && !self.exhausted.swap(true, Ordering::SeqCst)
                && let Some(observer) = self.observer.get()
            {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.audit_exhausted()));
            }
        }
        written
    }

    /// Appends the refusal `record`, unless `key` (the verified caller, or where an unverified
    /// one sits) has had a burst of the same refusal written in this window (see
    /// [`crate::refusals`]). Never called with the table locked.
    pub(crate) fn refuse(&self, mut record: AuditRecord, key: &str) {
        if let Some(suppressed) = self.refusals.admit(key, record.event, record.outcome) {
            record.suppressed = Some(suppressed);
            let _ = self.audit(&record);
        }
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
///
/// # Registration
///
/// A configured launcher is bound to the supervisor's own process or to a child the embedding
/// spawned and still holds; the operator only to the supervisor's own process. Nothing is ever
/// bound to the peer of a connection, and the operator is never minted for one:
///
/// ```no_run
/// # fn f(supervisor: &basemind_agents::Supervisor, child: &mut std::process::Child)
/// # -> Result<(), basemind_agents::SupervisorError> {
/// supervisor.register_launcher_in_process("lead")?;
/// supervisor.register_launcher_child("second", child)?;
/// supervisor.register_operator_in_process()?;
/// let operator = supervisor.operator()?;
/// # let _ = operator;
/// supervisor.unregister_launcher("second");
/// # Ok(())
/// # }
/// ```
///
/// ```compile_fail
/// # fn f(supervisor: &basemind_agents::Supervisor, peer: basemind_agents::Peer) {
/// let _ = supervisor.register_launcher("lead", peer);
/// # }
/// ```
///
/// ```compile_fail
/// # fn f(supervisor: &basemind_agents::Supervisor, peer: basemind_agents::Peer) {
/// let _ = supervisor.register_operator(peer);
/// # }
/// ```
///
/// ```compile_fail
/// # fn f(supervisor: &basemind_agents::Supervisor, peer: basemind_agents::Peer) {
/// let _ = supervisor.operator(peer);
/// # }
/// ```
///
/// ```compile_fail
/// # fn f(supervisor: &basemind_agents::Supervisor, child: &std::process::Child) {
/// let _ = supervisor.register_operator_child(child);
/// # }
/// ```
pub struct Supervisor {
    pub(crate) inner: Arc<Inner>,
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
    /// name, or a stop grace over [`MAX_STOP_GRACE_CEILING`](crate::MAX_STOP_GRACE_CEILING);
    /// and [`PolicyError::AuditReserve`] when `audit` cannot set aside, for the supervisor's
    /// life, the most its budget may write into the log's reserve (launches, exits, the stop of
    /// each session, allows and registrations; see [`AuditSink::reserve`]).
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
        let needed = crate::supervisor_reserve::demand(&policy, &configs);
        audit
            .reserve(needed)
            .map_err(|_| PolicyError::AuditReserve { needed })?;
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
                registrations: AtomicU32::new(0),
                allows: Arc::default(),
                exhausted: Arc::default(),
                refusals: Refusals::default(),
                registering: Mutex::new(()),
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

    /// Whether the audit log is exhausted: a record the supervisor's reserve was claimed for
    /// could not be written (a write error, or another writer filled the file). From then on
    /// every launch is refused with [`SupervisorError::AuditExhausted`] and no allow is given;
    /// sessions already running go on, and their stops and exits are attempted. The observer
    /// hears it once ([`SessionObserver::audit_exhausted`]); the embedding should surface it and
    /// have the operator rotate the log before building a new supervisor.
    #[must_use]
    pub fn audit_exhausted(&self) -> bool {
        self.inner.exhausted.load(Ordering::SeqCst)
    }

    /// The policy in force.
    #[must_use]
    pub fn policy(&self) -> &LaunchPolicy {
        &self.inner.policy
    }

    /// How many audit records could not be written. A failed launch record refuses the launch
    /// and a failed record of an allow keeps the allow from the harness; the others (refusals,
    /// stops, exits, denials, approvals the policy settled) are counted here, and the sessions
    /// they are about run on.
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
        // The table is locked from admission to the slot's reservation, so concurrent launches
        // cannot overshoot a limit; no audit record is written while it is held.
        let mut table = inner.table();
        let mut admitted = admit(inner, &table, caller, flavor, &mut refusal);
        // An admitted caller's request is checked against the flavor's config before anything
        // is reserved or recorded as accepted, outside the lock since planning reads the file
        // system, and only for an admitted caller; admission is then checked again.
        if let Ok(first) = &admitted {
            let template = first.config;
            drop(table);
            let unplannable = inner.registry.plan(template, request).err();
            table = inner.table();
            admitted = match unplannable {
                Some(error) => Err(SupervisorError::Launch(error)),
                None => admit(inner, &table, caller, flavor, &mut refusal),
            };
        }
        let Admitted {
            launcher,
            lineage,
            launcher_name,
            depth,
            config,
        } = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                let key = Asker::new(&table, caller, None).key;
                let key = refusal.caller.clone().unwrap_or(key);
                drop(table);
                return Err(refuse(inner, refusal, &key, error));
            }
        };
        let id = table.next;
        let allowance = Allowance::new(
            inner.allows.clone(),
            inner.policy.budget().max_allows,
            inner.exhausted.clone(),
        );
        let prepared = LaunchToken::generate()
            .and_then(|token| Ok((token, crate::identity::random_bytes::<4>()?)))
            .map_err(SupervisorError::Token)
            .and_then(|(token, bytes)| {
                let agent = format!("{LAUNCHED_PREFIX}{}-{id}-{}", flavor.id(), crate::identity::hex(&bytes));
                let config = launched_config(inner, config, &agent, depth, token.clone(), &allowance)?;
                Ok((token, agent, config))
            });
        let (token, agent, config) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                drop(table);
                let key = refusal.caller.clone().unwrap_or_default();
                return Err(refuse(inner, refusal, &key, error));
            }
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
        table.next += 1;
        table.launches = table.launches.saturating_add(1);
        let root = config.root.clone();
        table.sessions.insert(
            id,
            Entry {
                flavor,
                agent: agent.clone(),
                launcher,
                lineage,
                launcher_name,
                depth,
                token,
                leader: None,
                state: SessionState::Starting,
                stop_requested: false,
                started: SystemTime::now(),
                root,
                handle: None,
                detached: Arc::default(),
                approvals: HashMap::new(),
                allowance,
                pump: None,
            },
        );
        drop(table);

        // From here the slot is given back unless the pump takes the session: on a launch
        // record that cannot be written, an adapter error, a failed thread spawn, or a panic
        // in the adapter.
        let mut reservation = Reservation::new(inner, id, refusal);
        if let Err(error) = inner.audit(&record) {
            let error = SupervisorError::Audit(error);
            reservation.outcome = error.code();
            return Err(error);
        }
        let session = match inner.registry.launch(&config, request) {
            Ok(session) => session,
            Err(error) => {
                let error = SupervisorError::Launch(error);
                reservation.outcome = error.code();
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
        // The harness is unreaped until the session reports it finished, so its pid is still
        // the process the session started.
        entry.leader = procinfo::info(i32::try_from(handle.pid()).unwrap_or(-1)).map(|leader| leader.id());
        if let Some(leader) = entry.leader {
            entry.detached.follow(leader);
        } else if crate::policy::caveats(flavor).contains(&crate::policy::Caveat::DetachedServer) {
            // Its detached processes could not be followed: the session does not run unwatched.
            entry.stop_requested = true;
        }
        let detached = entry.detached.clone();
        let pump = std::thread::Builder::new()
            .name(format!("basemind-session-{id}"))
            .spawn({
                let inner = self.inner.clone();
                move || supervise(&inner, id, session, &detached, flavor)
            });
        let pump = match pump {
            Ok(pump) => pump,
            Err(error) => {
                // The closure, and the session in it, is dropped: its process group is killed.
                drop(table);
                let error = SupervisorError::Thread(error);
                reservation.outcome = error.code();
                return Err(error);
            }
        };
        let entry = table
            .sessions
            .get_mut(&id)
            .expect("only the pump removes a started entry");
        entry.pump = Some(pump);
        if entry.stop_requested {
            entry.state = SessionState::Stopping;
            handle.stop();
        } else {
            entry.state = SessionState::Running;
        }
        drop(table);
        reservation.disarm();
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
        // A timeout too large for the clock waits without a deadline.
        let deadline = Instant::now().checked_add(timeout);
        let mut table = self.inner.table();
        loop {
            let state = table.sessions.get(&id.0)?.state;
            if !state.live() {
                return Some(state);
            }
            table = match self.inner.until(table, deadline) {
                Some(table) => table,
                None => return Some(state),
            };
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
        let asker = Asker::new(&table, caller, resolved.as_ref().ok());
        let mut record = AuditRecord {
            session: Some(id.0),
            ..AuditRecord::new(AuditEvent::Stop, "requested")
        };
        asker.stamp(&mut record);
        let entry = match owned(&mut table, resolved, id) {
            Ok(entry) => entry,
            Err(error) => {
                record.outcome = error.code();
                drop(table);
                inner.refuse(record, &asker.key);
                return Err(error);
            }
        };
        record.flavor = Some(entry.flavor.id());
        record.agent = Some(entry.agent.clone());
        // Each session is asked to stop once, so a stop that is written in full (and may use
        // the log's reserve) is written at most once per session.
        if !entry.state.live() {
            record.outcome = "already_finished";
        } else if !stoppable(entry) {
            record.outcome = "already_stopping";
        } else {
            request_stop(entry);
        }
        // Every session below this one stops with it: once it is gone its token stops
        // resolving, and nothing but the operator could stop them.
        let mut cascaded = Vec::new();
        for (below, entry) in &mut table.sessions {
            if stoppable(entry) && entry.lineage.contains(&id.0) {
                request_stop(entry);
                cascaded.push(AuditRecord {
                    caller: record.caller.clone(),
                    session: Some(*below),
                    flavor: Some(entry.flavor.id()),
                    agent: Some(entry.agent.clone()),
                    ..AuditRecord::new(AuditEvent::Stop, "cascaded")
                });
            }
        }
        drop(table);
        // Only a stop that stopped something is written in full; a repeated one could be
        // looped on, so it is coalesced like a refusal.
        if record.outcome == "requested" {
            let _ = inner.audit(&record);
        } else {
            inner.refuse(record, &asker.key);
        }
        for record in &cascaded {
            let _ = inner.audit(record);
        }
        Ok(())
    }

    /// Stops every live session, as the supervisor itself.
    pub fn stop_all(&self) {
        let inner = &self.inner;
        let mut stopped = Vec::new();
        let mut table = inner.table();
        for (id, entry) in &mut table.sessions {
            if stoppable(entry) {
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
    /// the operator or launched the session, [`SupervisorError::Audit`] for an allow whose
    /// record could not be written (it is then not delivered), and the session's own errors (a
    /// finished session, a full control queue) as [`SupervisorError::Launch`].
    pub fn answer(&self, caller: &Caller, id: SessionId, approval: &str, allow: bool) -> Result<(), SupervisorError> {
        let inner = &self.inner;
        let mut table = inner.table();
        let resolved = resolve(&inner.policy, &table, caller);
        let asker = Asker::new(&table, caller, resolved.as_ref().ok());
        let entry = match owned(&mut table, resolved, id) {
            Ok(entry) => entry,
            Err(error) => {
                drop(table);
                let mut record = AuditRecord {
                    session: Some(id.0),
                    ..AuditRecord::new(AuditEvent::ApprovalAnswer, error.code())
                };
                asker.stamp(&mut record);
                inner.refuse(record, &asker.key);
                return Err(error);
            }
        };
        // An id the session did not issue is the caller's own text: it is neither recorded nor
        // delivered.
        let Some(kind) = entry.approvals.get(approval).copied() else {
            let flavor = entry.flavor.id();
            drop(table);
            let mut record = AuditRecord {
                session: Some(id.0),
                flavor: Some(flavor),
                ..AuditRecord::new(AuditEvent::ApprovalAnswer, "")
            };
            asker.stamp(&mut record);
            return Err(refuse(inner, record, &asker.key, SupervisorError::UnknownApproval));
        };
        let recorded_id = approval.to_owned();
        let handle = entry.handle.clone();
        let flavor = entry.flavor.id();
        let allowance = entry.allowance.clone();
        drop(table);
        let record = |outcome, allowed| {
            let mut record = AuditRecord {
                session: Some(id.0),
                flavor: Some(flavor),
                approval: Some(AuditApproval {
                    id: recorded_id.clone(),
                    kind,
                    allowed,
                    source: "caller",
                }),
                ..AuditRecord::new(AuditEvent::ApprovalAnswer, outcome)
            };
            asker.stamp(&mut record);
            record
        };
        // An allow is on the record before it reaches the harness, or it does not reach it: the
        // request stays pending and is denied when its turn or the session ends. A denial
        // goes through whatever the log says.
        if allow {
            // An allow past the session's or the supervisor's allowance is refused, not
            // delivered: the request stays pending for a denial.
            if !allowance.take() {
                let error = SupervisorError::AllowsExhausted;
                inner.refuse(record(error.code(), false), &asker.key);
                return Err(error);
            }
            inner.audit(&record("allowed", true)).map_err(SupervisorError::Audit)?;
        }
        let answered = handle
            .ok_or(LaunchError::SessionClosed)
            .and_then(|handle| handle.answer(approval, allow))
            .map_err(SupervisorError::Launch);
        // A denial or a failed answer could be looped on: coalesced like a refusal.
        match (&answered, allow) {
            (Err(error), _) => inner.refuse(record(error.code(), false), &asker.key),
            (Ok(()), false) => inner.refuse(record("denied", false), &asker.key),
            (Ok(()), true) => {}
        }
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
        for pending in self.inner.refusals.drain() {
            let _ = self.inner.audit(&AuditRecord {
                caller: Some(pending.caller),
                suppressed: Some(pending.suppressed),
                ..AuditRecord::new(pending.event, pending.outcome)
            });
        }
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
        let deadline = Instant::now().checked_add(timeout);
        let mut table = self.inner.table();
        loop {
            if table.live().next().is_none() {
                return true;
            }
            table = match self.inner.until(table, deadline) {
                Some(table) => table,
                None => return false,
            };
        }
    }
}

/// Whether `entry` runs and has not been asked to stop yet.
fn stoppable(entry: &Entry) -> bool {
    matches!(entry.state, SessionState::Starting | SessionState::Running) && !entry.stop_requested
}

pub(crate) fn request_stop(entry: &mut Entry) {
    match &entry.handle {
        Some(handle) => {
            entry.state = SessionState::Stopping;
            handle.stop();
        }
        // Still starting: the launch stops it as soon as it has the handle.
        None => entry.stop_requested = true,
    }
}

#[cfg(all(test, unix))]
#[path = "supervisor_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "supervisor_attack_tests.rs"]
mod attack_tests;

#[cfg(all(test, unix))]
#[path = "supervisor_identity_tests.rs"]
mod identity_tests;

#[cfg(all(test, unix))]
#[path = "supervisor_audit_tests.rs"]
mod audit_tests;
