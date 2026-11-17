//! The public types of the [`Supervisor`](crate::Supervisor): session ids, callers, session
//! snapshots, the observer hook and the refusal error.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::error::LaunchError;
use crate::event::{Event, FinishStatus};
use crate::harness::HarnessKind;
use crate::identity::AgentIdentity;
use crate::peer::Peer;
use crate::policy::Caveat;
use crate::supervisor_codes::launch_error_code;

/// A session's id, unique within its supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub(crate) u64);

impl SessionId {
    /// The id as a number.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// The id `value`, as an engine read it back from a tool call.
    #[must_use]
    pub fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Who is asking the supervisor for something.
///
/// A configured launcher names its principal; a launched agent presents the launch token it
/// was given in [`ENV_LAUNCH_TOKEN`](crate::ENV_LAUNCH_TOKEN). Either way the caller carries the
/// [`Peer`] the kernel reported for its connection, and the supervisor decides by that process,
/// not by the claim: a process inside a live session, or below one, is that session whatever it
/// claims; a process below the supervisor in no session is nobody; only a process outside the
/// supervisor's tree is the principal it names. `Debug` never shows a token.
///
/// There is no public constructor for the operator: only
/// [`Supervisor::operator`](crate::Supervisor::operator) makes one, for a peer outside the
/// supervisor's tree.
///
/// ```compile_fail
/// let operator = basemind_agents::Caller::operator();
/// ```
#[derive(Clone)]
pub struct Caller {
    pub(crate) kind: CallerKind,
    pub(crate) peer: Peer,
}

#[derive(Clone)]
pub(crate) enum CallerKind {
    Operator,
    Principal(String),
    Token(String),
}

impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.kind {
            CallerKind::Operator => "operator".to_owned(),
            CallerKind::Principal(principal) => format!("principal {principal:?}"),
            CallerKind::Token(_) => "launch token <redacted>".to_owned(),
        };
        f.debug_struct("Caller")
            .field("kind", &kind)
            .field("peer", &self.peer.pid())
            .finish()
    }
}

impl Caller {
    /// A principal of the comms identity model on the connection of `peer`, admitted to launch
    /// when the policy names it and `peer` is outside the supervisor's tree.
    ///
    /// # Errors
    ///
    /// [`LaunchError::InvalidIdentity`] when `principal` fails the principal grammar.
    pub fn principal(principal: &str, peer: Peer) -> Result<Self, LaunchError> {
        AgentIdentity::new(principal)?;
        Ok(Self {
            kind: CallerKind::Principal(principal.to_owned()),
            peer,
        })
    }

    /// A launched agent on the connection of `peer`, by the token in its
    /// `BASEMIND_LAUNCH_TOKEN`. The token has to be the session's that `peer` belongs to.
    #[must_use]
    pub fn launched(token: impl Into<String>, peer: Peer) -> Self {
        Self {
            kind: CallerKind::Token(token.into()),
            peer,
        }
    }
}

/// Where a session is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionState {
    /// Admitted and being started.
    Starting,
    /// Running.
    Running,
    /// Told to stop, and winding down.
    Stopping,
    /// Over; the harness's process group is gone.
    Finished(FinishStatus),
}

impl SessionState {
    pub(crate) fn live(self) -> bool {
        !matches!(self, Self::Finished(_))
    }
}

/// A snapshot of one session.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionStatus {
    /// The session.
    pub id: SessionId,
    /// The harness flavor.
    pub flavor: HarnessKind,
    /// The launched agent's principal.
    pub agent: String,
    /// The principal that launched it: a configured launcher or a launched agent.
    pub launcher: String,
    /// The launched agent's depth: 1 below a configured launcher.
    pub depth: u32,
    /// Where it is in its life.
    pub state: SessionState,
    /// When it was admitted.
    pub started: SystemTime,
    /// The harness's process id, also its process group id, while it runs.
    pub pid: Option<u32>,
    /// The root it was launched in, as configured.
    pub root: PathBuf,
    /// The flavor's known caveats.
    pub caveats: &'static [Caveat],
}

/// What a successful launch returns.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Launched {
    /// The session.
    pub id: SessionId,
    /// The principal the launched agent acts as.
    pub agent: String,
    /// Its depth.
    pub depth: u32,
    /// The flavor's known caveats.
    pub caveats: &'static [Caveat],
}

/// Receives every event of every session, on the thread that supervises that session. It must
/// return quickly: the session's events wait for it.
pub trait SessionObserver: Send + Sync {
    /// `event` happened in `session`.
    fn event(&self, session: SessionId, event: &Event);
}

/// Why the supervisor refused a request. Every variant has a stable [`code`](Self::code), the
/// form the audit log and a model-facing reply use.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SupervisorError {
    /// The policy allows no launch at all.
    #[error("agent launch is disabled")]
    Disabled,
    /// The policy does not allow this harness, or no launch config is given for it.
    #[error("launching {0} is not allowed")]
    FlavorNotAllowed(&'static str),
    /// The caller is not a launcher the policy names, nor a live launched session.
    #[error("the caller may not launch or control agents")]
    CallerNotAdmitted,
    /// The presented launch token belongs to no live session.
    #[error("the launch token is not valid")]
    UnknownToken,
    /// The caller's process could not be placed: it is gone, its pid was reused, or its
    /// ancestry could not be read.
    #[error("the caller's process could not be verified")]
    PeerUnverified,
    /// The caller's process does not belong to the session its launch token names.
    #[error("the launch token does not belong to the caller's process")]
    PeerMismatch,
    /// The launch would sit deeper than the policy's `max_depth`.
    #[error("depth {depth} is over the limit of max_depth = {max_depth}")]
    DepthExceeded {
        /// The depth the launched agent would have.
        depth: u32,
        /// The policy's limit.
        max_depth: u32,
    },
    /// The supervisor already runs the policy's `max_concurrent` sessions.
    #[error("{max_concurrent} sessions are running, the limit of max_concurrent")]
    ConcurrencyExceeded {
        /// The policy's limit.
        max_concurrent: u32,
    },
    /// The launcher already runs the policy's `max_children` sessions.
    #[error("the launcher runs {max_children} sessions, the limit of max_children")]
    FanOutExceeded {
        /// The policy's limit.
        max_children: u32,
    },
    /// The budget's `max_launches` launches have been made.
    #[error("the budget of max_launches = {max_launches} launches is spent")]
    BudgetExhausted {
        /// The budget's limit.
        max_launches: u32,
    },
    /// The launch record could not be written, so nothing was launched.
    #[error("the audit log could not be written: {0}")]
    Audit(#[source] std::io::Error),
    /// No launch token could be made, so nothing was launched.
    #[error("no launch token could be made: {0}")]
    Token(#[source] std::io::Error),
    /// The thread that supervises a session could not be started, so the harness was killed.
    #[error("the session's supervising thread could not be started: {0}")]
    Thread(#[source] std::io::Error),
    /// The adapter refused the launch or could not start it, or the session refused a control.
    /// Shown by its code only: the adapter's error can name paths of the host; it stays
    /// available as the source.
    #[error("the launch was refused: {}", launch_error_code(.0))]
    Launch(#[from] LaunchError),
    /// No session of that id is known.
    #[error("no session {0}")]
    UnknownSession(SessionId),
    /// The caller did not launch the session and is not the operator.
    #[error("session {0} was not launched by the caller")]
    NotOwner(SessionId),
    /// The session has no pending approval request of that id.
    #[error("no pending approval request of that id")]
    UnknownApproval,
}

impl SupervisorError {
    /// A stable code naming the refusal, without paths or harness text.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::FlavorNotAllowed(_) => "flavor_not_allowed",
            Self::CallerNotAdmitted => "caller_not_admitted",
            Self::UnknownToken => "unknown_token",
            Self::PeerUnverified => "peer_unverified",
            Self::PeerMismatch => "peer_mismatch",
            Self::DepthExceeded { .. } => "depth_exceeded",
            Self::ConcurrencyExceeded { .. } => "concurrency_exceeded",
            Self::FanOutExceeded { .. } => "fan_out_exceeded",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::Audit(_) => "audit_failed",
            Self::Token(_) => "token_failed",
            Self::Thread(_) => "thread_failed",
            Self::Launch(error) => launch_error_code(error),
            Self::UnknownSession(_) => "unknown_session",
            Self::NotOwner(_) => "not_owner",
            Self::UnknownApproval => "unknown_approval",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_refusal_shows_its_code_and_no_path() {
        let error = SupervisorError::Launch(LaunchError::WorkingDirChanged(PathBuf::from("/home/someone/secret")));
        let shown = error.to_string();
        assert!(!shown.contains("/home/someone"), "{shown}");
        assert!(shown.contains(error.code()), "{shown}");
    }
}
