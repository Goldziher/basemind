//! Who may launch which harness, how deep, how many at once and for how long: the
//! [`LaunchPolicy`] a [`Supervisor`](crate::Supervisor) enforces before any adapter runs.
//!
//! The policy is trusted configuration, validated when it is built. Its [`Default`] denies
//! every launch, so launching is off until the operator's configuration names a harness and a
//! principal that may launch it.

use std::time::Duration;

use crate::harness::{AcpFlavor, HarnessKind};
use crate::identity::AgentIdentity;
use crate::spec::{BypassOptIn, PermissionMode, UnverifiedModeOptIn};

/// The deepest launch tree a policy may allow.
pub const MAX_DEPTH_CEILING: u32 = 8;
/// The most live sessions a policy may allow at once. A session holds up to about 200 MiB in
/// the worst case (see the crate's ADR draft), so the ceiling bounds the launcher's memory.
pub const MAX_CONCURRENT_CEILING: u32 = 32;
/// The longest wall time a policy may allow a session.
pub const MAX_WALL_TIME_CEILING: Duration = Duration::from_secs(24 * 60 * 60);
/// The longest stop grace a launch config may give a session. A dropped supervisor waits for
/// the longest one, so it bounds how long a drop can take.
pub const MAX_STOP_GRACE_CEILING: Duration = Duration::from_secs(60);
/// The prefix of every principal a supervisor gives a launched agent. A launcher principal may
/// not start with it, so a launched agent can never be mistaken for a configured launcher.
pub const LAUNCHED_PREFIX: &str = "launched-";

/// A known behaviour of a harness that the policy cannot switch off and the operator should
/// weigh before allowing it. Reported on the flavor's policy and on each of its sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Caveat {
    /// The harness runs its server, MCP servers and plugins in a process group of their own,
    /// which a kill of the session's group does not reach: `opencode acp` 2.3.0 starts
    /// `opencode serve --stdio` that way, and it outlived the group by about 1.7 s with its
    /// input closed and about 3 s with it open. The supervisor records such groups while the
    /// session runs, waits for them after it ends and kills what is left.
    DetachedServer,
    /// The harness loads the user's own MCP servers and plugins from its user-level
    /// configuration, beside the injected one, even when the launch names none: OpenCode 2.3.0
    /// with an empty `mcpServers`, and Codex 0.169 from `~/.codex/config.toml`. What those
    /// servers can do is outside the launch's control.
    UserMcpServers,
}

impl Caveat {
    /// A stable lower-case id, used in the audit log.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::DetachedServer => "detached_server",
            Self::UserMcpServers => "user_mcp_servers",
        }
    }
}

/// The caveats known for harnesses of `kind`.
#[must_use]
pub fn caveats(kind: HarnessKind) -> &'static [Caveat] {
    match kind {
        HarnessKind::Acp(AcpFlavor::OpenCode) => &[Caveat::DetachedServer, Caveat::UserMcpServers],
        HarnessKind::Codex => &[Caveat::UserMcpServers],
        HarnessKind::ClaudeCode | HarnessKind::Acp(_) => &[],
    }
}

/// One harness flavor the policy allows, and the permission mode its launches run in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlavorPolicy {
    /// The harness flavor.
    pub kind: HarnessKind,
    /// The permission mode every launch of this flavor runs in; the flavor's
    /// [`LaunchConfig`](crate::LaunchConfig) has to name the same.
    pub permission_mode: PermissionMode,
    /// The consent for [`PermissionMode::Bypass`]: required for it, refused without it. It has
    /// to name this flavor and its bypass mode.
    pub bypass: Option<BypassOptIn>,
    /// The consent to run this ACP flavor in a mode nobody verified; the flavor's config has to
    /// carry the same one. It has to name this flavor and its permission mode.
    pub unverified_mode: Option<UnverifiedModeOptIn>,
    /// Whether the flavor's config may carry an
    /// [`ApprovalPolicy`](crate::ApprovalPolicy). The crate cannot see what a policy decides,
    /// and one that allows every request is a bypass, so it is consent like the opt-ins.
    /// Without it every approval request of the flavor is denied.
    pub approval_policy: bool,
    /// The MCP tools the flavor's config may pre-approve; its server's
    /// [`allow_tools`](crate::McpServerSpec::allow_tools) has to be within them.
    pub allow_tools: Vec<String>,
}

impl FlavorPolicy {
    /// `kind` in its supervised mode.
    #[must_use]
    pub fn supervised(kind: HarnessKind) -> Self {
        Self {
            kind,
            permission_mode: PermissionMode::Supervised,
            bypass: None,
            unverified_mode: None,
            approval_policy: false,
            allow_tools: Vec::new(),
        }
    }

    /// The known caveats of this flavor.
    #[must_use]
    pub fn caveats(&self) -> &'static [Caveat] {
        caveats(self.kind)
    }
}

/// A budget on what a supervisor spends over its lifetime and per session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// How many launches the supervisor accepts in all; a further one is refused.
    pub max_launches: u32,
    /// The most turns any one session may run; a config's cap above it is lowered to it.
    pub max_turns: u32,
}

/// The unvalidated form of a [`LaunchPolicy`], as the operator's configuration states it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PolicySpec {
    /// The harness flavors that may be launched. Empty, nothing may.
    pub flavors: Vec<FlavorPolicy>,
    /// The principals that may launch at the top of a tree. Empty, nobody may.
    pub launchers: Vec<String>,
    /// The deepest a launched agent may sit: 1 lets only a configured launcher launch, 2 lets
    /// the agents it launched launch once more, and so on, up to [`MAX_DEPTH_CEILING`].
    pub max_depth: u32,
    /// The most sessions alive at once, across every launcher.
    pub max_concurrent: u32,
    /// The most live sessions one launcher (a principal or a launched session) may have.
    pub max_children: u32,
    /// The longest any session may run; a config's cap above it is lowered to it.
    pub wall_time: Duration,
    /// An optional lifetime and per-session budget.
    pub budget: Option<Budget>,
}

/// Why a [`PolicySpec`], or a launch config paired with it, was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyError {
    /// A flavor is named twice.
    #[error("harness {0} is named twice in the launch policy")]
    DuplicateFlavor(&'static str),
    /// A flavor's permission mode is a bypass and no opt-in names that flavor and its mode.
    #[error("harness {harness} may run in bypass mode only with an opt-in naming {harness} and its mode {mode:?}")]
    BypassWithoutOptIn {
        /// The flavor.
        harness: &'static str,
        /// The mode name the opt-in has to carry.
        mode: &'static str,
    },
    /// A flavor carries a bypass opt-in that does not match it, or that its mode does not use.
    #[error("the bypass opt-in for harness {0} does not name it and its bypass mode, or its mode is not bypass")]
    OptInMismatch(&'static str),
    /// A launcher principal does not satisfy the principal grammar.
    #[error("launcher {0:?} does not match [A-Za-z0-9_:-]{{1,128}}")]
    InvalidLauncher(String),
    /// A launcher principal starts with [`LAUNCHED_PREFIX`], which launched agents carry.
    #[error("launcher {0:?} starts with the prefix reserved for launched agents")]
    ReservedLauncher(String),
    /// A limit is zero while the policy allows launching.
    #[error("{0} is zero")]
    ZeroLimit(&'static str),
    /// A limit is above its ceiling.
    #[error("{limit} is above its ceiling of {ceiling}")]
    LimitTooLarge {
        /// The limit.
        limit: &'static str,
        /// Its ceiling.
        ceiling: u64,
    },
    /// The policy names launchers and limits but no flavor, or flavors but no launcher.
    #[error("a launch policy that allows anything needs at least one flavor and one launcher")]
    Incomplete,
    /// A launch config was given for a flavor the policy does not allow.
    #[error("a launch config is given for harness {0}, which the policy does not allow")]
    ConfigNotAllowed(&'static str),
    /// Two launch configs were given for one flavor.
    #[error("two launch configs are given for harness {0}")]
    DuplicateConfig(&'static str),
    /// A launch config disagrees with the policy for its flavor.
    #[error("the launch config for harness {harness} disagrees with the policy on {field}")]
    ConfigMismatch {
        /// The flavor.
        harness: &'static str,
        /// The field that differs.
        field: &'static str,
    },
}

/// A validated launch policy. [`LaunchPolicy::default`] allows nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchPolicy {
    spec: PolicySpec,
}

impl LaunchPolicy {
    /// Validates `spec`.
    ///
    /// An empty spec (no flavor, no launcher) is the disabled policy. Otherwise every flavor
    /// is named once, a bypass mode carries an opt-in naming its flavor and mode and any other
    /// mode carries none, every launcher matches the principal grammar and does not start with
    /// [`LAUNCHED_PREFIX`], and every limit is above zero and at most its ceiling, with
    /// `max_children` at most `max_concurrent`.
    ///
    /// # Errors
    ///
    /// The [`PolicyError`] naming the first rule `spec` breaks.
    pub fn new(spec: PolicySpec) -> Result<Self, PolicyError> {
        if spec.flavors.is_empty() && spec.launchers.is_empty() {
            return Ok(Self::default());
        }
        if spec.flavors.is_empty() || spec.launchers.is_empty() {
            return Err(PolicyError::Incomplete);
        }
        for (at, flavor) in spec.flavors.iter().enumerate() {
            if spec.flavors[..at].iter().any(|earlier| earlier.kind == flavor.kind) {
                return Err(PolicyError::DuplicateFlavor(flavor.kind.id()));
            }
            check_bypass(flavor)?;
            if flavor
                .unverified_mode
                .is_some_and(|opt_in| opt_in.harness != flavor.kind || opt_in.mode != flavor.permission_mode)
            {
                return Err(PolicyError::OptInMismatch(flavor.kind.id()));
            }
        }
        for launcher in &spec.launchers {
            AgentIdentity::new(launcher.as_str()).map_err(|_| PolicyError::InvalidLauncher(launcher.clone()))?;
            if launcher.starts_with(LAUNCHED_PREFIX) {
                return Err(PolicyError::ReservedLauncher(launcher.clone()));
            }
        }
        check_count("max_depth", spec.max_depth, MAX_DEPTH_CEILING)?;
        check_count("max_concurrent", spec.max_concurrent, MAX_CONCURRENT_CEILING)?;
        check_count("max_children", spec.max_children, spec.max_concurrent)?;
        if spec.wall_time.is_zero() {
            return Err(PolicyError::ZeroLimit("wall_time"));
        }
        if spec.wall_time > MAX_WALL_TIME_CEILING {
            return Err(PolicyError::LimitTooLarge {
                limit: "wall_time",
                ceiling: MAX_WALL_TIME_CEILING.as_secs(),
            });
        }
        if let Some(budget) = spec.budget {
            check_count("budget.max_launches", budget.max_launches, u32::MAX)?;
            check_count("budget.max_turns", budget.max_turns, u32::MAX)?;
        }
        Ok(Self { spec })
    }

    /// Whether the policy allows any launch at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.spec.flavors.is_empty()
    }

    /// The rule for `kind`, when the policy allows it.
    #[must_use]
    pub fn flavor(&self, kind: HarnessKind) -> Option<&FlavorPolicy> {
        self.spec.flavors.iter().find(|flavor| flavor.kind == kind)
    }

    /// The allowed flavors.
    #[must_use]
    pub fn flavors(&self) -> &[FlavorPolicy] {
        &self.spec.flavors
    }

    /// The principals that may launch at the top of a tree, once each is registered to a
    /// process ([`Supervisor::register_launcher`](crate::Supervisor::register_launcher)).
    #[must_use]
    pub fn launchers(&self) -> &[String] {
        &self.spec.launchers
    }

    /// Whether `principal` may launch at the top of a tree.
    #[must_use]
    pub fn admits(&self, principal: &str) -> bool {
        self.spec.launchers.iter().any(|launcher| launcher == principal)
    }

    /// The deepest a launched agent may sit.
    #[must_use]
    pub fn max_depth(&self) -> u32 {
        self.spec.max_depth
    }

    /// The most sessions alive at once.
    #[must_use]
    pub fn max_concurrent(&self) -> u32 {
        self.spec.max_concurrent
    }

    /// The most live sessions one launcher may have.
    #[must_use]
    pub fn max_children(&self) -> u32 {
        self.spec.max_children
    }

    /// The longest any session may run.
    #[must_use]
    pub fn wall_time(&self) -> Duration {
        self.spec.wall_time
    }

    /// The budget, if any.
    #[must_use]
    pub fn budget(&self) -> Option<Budget> {
        self.spec.budget
    }
}

/// Checks `config` against the rule for its flavor: every field that widens what the launched
/// agent may do unasked has to be what the policy grants, and the stop grace within its
/// ceiling.
pub(crate) fn check_config(rule: &FlavorPolicy, config: &crate::spec::LaunchConfig) -> Result<(), PolicyError> {
    let mismatch = |field| PolicyError::ConfigMismatch {
        harness: rule.kind.id(),
        field,
    };
    if config.permission_mode != rule.permission_mode {
        return Err(mismatch("permission_mode"));
    }
    if config.bypass != rule.bypass {
        return Err(mismatch("bypass"));
    }
    if config.unverified_mode != rule.unverified_mode {
        return Err(mismatch("unverified_mode"));
    }
    if config.approval_policy.is_some() && !rule.approval_policy {
        return Err(mismatch("approval_policy"));
    }
    let pre_approved = config.mcp_server.iter().flat_map(|server| &server.allow_tools);
    if pre_approved.into_iter().any(|tool| !rule.allow_tools.contains(tool)) {
        return Err(mismatch("allow_tools"));
    }
    if config.limits.stop_grace > MAX_STOP_GRACE_CEILING {
        return Err(mismatch("stop_grace"));
    }
    Ok(())
}

/// A bypass mode needs an opt-in naming the flavor and its own mode name; any other mode may
/// carry none, so a stray opt-in cannot sit in the policy waiting for a mode change.
fn check_bypass(flavor: &FlavorPolicy) -> Result<(), PolicyError> {
    let kind = flavor.kind;
    let bypass = flavor.permission_mode == PermissionMode::Bypass;
    match (&flavor.bypass, bypass) {
        (None, false) => Ok(()),
        (None, true) => Err(PolicyError::BypassWithoutOptIn {
            harness: kind.id(),
            mode: kind.bypass_mode().unwrap_or("<the session mode id>"),
        }),
        (Some(_), false) => Err(PolicyError::OptInMismatch(kind.id())),
        (Some(opt_in), true) => {
            let names_mode = match kind.bypass_mode() {
                Some(mode) => opt_in.mode == mode,
                None => kind == HarnessKind::Acp(AcpFlavor::Generic) && !opt_in.mode.is_empty(),
            };
            if opt_in.harness == kind && names_mode {
                Ok(())
            } else {
                Err(PolicyError::OptInMismatch(kind.id()))
            }
        }
    }
}

fn check_count(limit: &'static str, value: u32, ceiling: u32) -> Result<(), PolicyError> {
    if value == 0 {
        return Err(PolicyError::ZeroLimit(limit));
    }
    if value > ceiling {
        return Err(PolicyError::LimitTooLarge {
            limit,
            ceiling: u64::from(ceiling),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
