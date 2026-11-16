//! The typed error every fallible operation in this crate returns.

use std::path::PathBuf;

/// Why a launch was refused, could not start, or failed while running.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LaunchError {
    /// The agent id does not satisfy the principal grammar of `docs/comms-v2.md`.
    #[error("agent id {0:?} does not match [A-Za-z0-9_:-]{{1,128}}")]
    InvalidIdentity(String),
    /// The sub-name does not satisfy the sub-name grammar, or the composed id is too long.
    #[error("sub-name {0:?} does not match [a-z0-9_-]{{1,32}} or makes the id longer than 128 bytes")]
    InvalidSubName(String),
    /// The host does not satisfy `[a-z0-9]{1,16}`, the form the comms resolver reduces to.
    #[error("host {0:?} does not match [a-z0-9]{{1,16}}")]
    InvalidHost(String),
    /// The working directory resolves to a path outside the confinement root.
    #[error("working directory {dir} is outside the root {root}")]
    WorkingDirOutsideRoot {
        /// The canonical working directory that was requested.
        dir: PathBuf,
        /// The canonical root it had to stay inside.
        root: PathBuf,
    },
    /// The request's working directory is absolute; it has to be relative to the root.
    #[error("working directory {0} is not relative to the root")]
    WorkingDirNotRelative(PathBuf),
    /// A path the harness has to receive in its protocol is not valid UTF-8.
    #[error("path {0:?} is not valid UTF-8")]
    NonUtf8Path(PathBuf),
    /// The working directory was replaced between the check and the spawn: the child's
    /// directory after changing into it was not the checked canonical path.
    #[error("working directory {0} changed before the harness started")]
    WorkingDirChanged(PathBuf),
    /// The harness executable or the MCP server's command cannot be run safely: it is a
    /// relative path, it is not on the launcher's `PATH`, or it lies inside the root.
    #[error("cannot run {path}: {reason}")]
    InvalidExecutable {
        /// The path as configured.
        path: PathBuf,
        /// Why it was refused.
        reason: &'static str,
    },
    /// A path of the launch could not be resolved.
    #[error("cannot resolve {path}: {source}")]
    Path {
        /// The path as given.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A permission bypass was requested without an opt-in naming this harness and its mode.
    #[error("permission bypass for {harness} needs an explicit opt-in naming that harness and mode {mode:?}")]
    BypassNotAuthorized {
        /// The harness the launch targets.
        harness: &'static str,
        /// The mode name the opt-in has to carry.
        mode: String,
    },
    /// An argument the adapter built would enable a permission bypass the launch does not run in.
    #[error("argument {0:?} enables a permission bypass and no opt-in covers it")]
    BypassArgument(String),
    /// The request's model or effort is outside its grammar.
    #[error("argument {argument:?} is not allowed for {harness}")]
    ArgumentNotAllowed {
        /// The harness the launch targets.
        harness: &'static str,
        /// The offending value.
        argument: String,
    },
    /// The ACP agent's behaviour in this permission mode was not verified to ask before it
    /// acts, and the config carries no
    /// [`UnverifiedModeOptIn`](crate::UnverifiedModeOptIn) for that harness and mode.
    #[error("{harness} in {mode:?} mode was not verified to ask before acting; it needs an explicit opt-in")]
    UnverifiedMode {
        /// The harness the launch targets.
        harness: &'static str,
        /// The permission mode requested.
        mode: crate::PermissionMode,
    },
    /// The harness has no equivalent of the requested feature.
    #[error("{harness} does not support {feature}")]
    Unsupported {
        /// The harness the launch targets.
        harness: &'static str,
        /// What was asked of it.
        feature: &'static str,
    },
    /// The MCP server name cannot be written into every harness's configuration.
    #[error("MCP server name {0:?} does not match [A-Za-z0-9_-]{{1,64}}")]
    InvalidMcpServerName(String),
    /// A pre-approved MCP tool name is not `[A-Za-z0-9_-]{1,64}`.
    #[error("MCP tool name {0:?} does not match [A-Za-z0-9_-]{{1,64}}")]
    InvalidMcpToolName(String),
    /// An environment addition, of the config or of its MCP server, has a name a launch config
    /// may not set: see [`LaunchConfig::env`](crate::LaunchConfig::env).
    #[error("environment variable {0:?} may not be set by a launch config")]
    EnvNotAllowed(String),
    /// The working directory or an ancestor of it holds a project file through which the
    /// harness would load permissions, hooks or MCP servers the launch does not control.
    #[error("{harness} would load the project configuration at {path}")]
    ProjectConfig {
        /// The harness the launch targets.
        harness: &'static str,
        /// The file or directory found.
        path: PathBuf,
    },
    /// A limit is zero or too long to schedule.
    #[error("invalid limits: {0}")]
    InvalidLimits(&'static str),
    /// No adapter is registered for the harness kind.
    #[error("no adapter is registered for {0}")]
    UnknownHarness(&'static str),
    /// The harness executable could not be started.
    #[error("cannot start {executable}: {source}")]
    Spawn {
        /// The executable that was run.
        executable: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Reading from or writing to the harness failed.
    #[error("i/o with the harness failed: {0}")]
    Io(#[from] std::io::Error),
    /// The harness sent something its protocol does not allow, or reported a failure.
    #[error("harness protocol error: {0}")]
    Protocol(String),
    /// The session already ran the number of turns its limits allow.
    #[error("turn limit of {0} reached")]
    TurnLimit(u32),
    /// The session's control queue is full; the harness is not taking control messages as
    /// fast as they are sent.
    #[error("the session's control queue is full")]
    ControlQueueFull,
    /// The session has finished and accepts no more control.
    #[error("the session has finished")]
    SessionClosed,
}
