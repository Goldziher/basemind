//! What a [`Supervisor`](crate::Supervisor) decides before it acts: whether a launch is
//! admitted and as whom, the config the launched agent runs under, and whether a caller may
//! control a session.

use crate::audit::AuditRecord;
use crate::error::LaunchError;
use crate::harness::HarnessKind;
use crate::identity::{AgentIdentity, LaunchToken};
use crate::spec::LaunchConfig;
use crate::supervisor::{Entry, Inner, Launcher, Resolved, SessionId, SessionState, SupervisorError, Table};
use crate::supervisor_identity::{coalescing_key, resolve};
use crate::supervisor_types::{Caller, CallerKind};

/// Records `error` as the refusal's outcome, writes it coalesced under `key`, and returns the
/// error.
pub(crate) fn refuse(inner: &Inner, mut refusal: AuditRecord, key: &str, error: SupervisorError) -> SupervisorError {
    refusal.outcome = error.code();
    inner.refuse(refusal, key);
    error
}

/// Who a record says asked: the verified name once the caller resolved; before that, nobody,
/// with what it claimed (never a token) and its process kept apart.
pub(crate) struct Asker {
    caller: Option<String>,
    claimed: Option<String>,
    peer: u32,
    /// The key the caller's refusals are coalesced under.
    pub key: String,
}

impl Asker {
    pub fn new(table: &Table, caller: &Caller, resolved: Option<&Resolved>) -> Self {
        let peer = caller.peer.pid();
        match resolved {
            Some(resolved) => {
                let name = name_of(table, resolved);
                Self {
                    caller: Some(name.clone()),
                    claimed: None,
                    peer,
                    key: name,
                }
            }
            None => Self {
                caller: None,
                claimed: match &caller.kind {
                    CallerKind::Operator => Some("operator".to_owned()),
                    CallerKind::Principal(principal) => Some(principal.clone()),
                    CallerKind::Token(_) => None,
                },
                peer,
                key: coalescing_key(table, &caller.peer),
            },
        }
    }

    /// Writes who asked into `record`.
    pub fn stamp(&self, record: &mut AuditRecord) {
        record.caller.clone_from(&self.caller);
        record.claimed.clone_from(&self.claimed);
        record.peer = Some(self.peer);
    }
}

/// Every check a launch passes before anything is started: the policy is on, the caller is
/// admitted, the flavor is allowed and configured, and depth, budget, concurrency and fan-out
/// are within their limits. Fills in what the refusal record learns on the way.
pub(crate) fn admit<'a>(
    inner: &'a Inner,
    table: &Table,
    caller: &Caller,
    flavor: HarnessKind,
    refusal: &mut AuditRecord,
) -> Result<Admitted<'a>, SupervisorError> {
    let policy = &inner.policy;
    if !policy.enabled() {
        return Err(SupervisorError::Disabled);
    }
    // Until the caller resolves, a refusal names nobody and reports what was claimed.
    Asker::new(table, caller, None).stamp(refusal);
    let resolved = resolve(policy, table, caller)?;
    Asker::new(table, caller, Some(&resolved)).stamp(refusal);
    let (launcher, caller_depth, lineage) = match resolved {
        Resolved::Operator => return Err(SupervisorError::CallerNotAdmitted),
        Resolved::Top(principal) => (Launcher::Principal(principal), 0, Vec::new()),
        Resolved::Session(id) => {
            let parent = &table.sessions[&id];
            // A session told to stop, or not yet running, starts nothing.
            if parent.state != SessionState::Running {
                return Err(SupervisorError::CallerNotAdmitted);
            }
            let mut lineage = parent.lineage.clone();
            lineage.push(id);
            (Launcher::Session(id), parent.depth, lineage)
        }
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
    Ok(Admitted {
        launcher,
        lineage,
        launcher_name,
        depth,
        config,
    })
}

/// What [`admit`] decided about a launch.
pub(crate) struct Admitted<'a> {
    pub launcher: Launcher,
    pub lineage: Vec<u64>,
    pub launcher_name: String,
    pub depth: u32,
    pub config: &'a LaunchConfig,
}

/// The flavor's config as the launched agent runs it: its own principal (keeping the config's
/// host), its depth and token, and limits lowered to the policy's wall time and budget.
pub(crate) fn launched_config(
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

/// The name a resolved caller is recorded under.
pub(crate) fn name_of(table: &Table, resolved: &Resolved) -> String {
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
/// A stop of a launcher reaches the sessions below it (see [`Supervisor::stop`]).
pub(crate) fn owned(
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
