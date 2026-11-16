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
    /// The working directory resolves to a path outside the confinement root.
    #[error("working directory {dir} is outside the root {root}")]
    WorkingDirOutsideRoot {
        /// The canonical working directory that was requested.
        dir: PathBuf,
        /// The canonical root it had to stay inside.
        root: PathBuf,
    },
    /// A path of the spec could not be resolved.
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
    /// An extra argument is not on the harness's allow-list, is a positional, repeats an
    /// allowed flag or carries a value outside its grammar.
    #[error("argument {argument:?} is not allowed for {harness}")]
    ArgumentNotAllowed {
        /// The harness the launch targets.
        harness: &'static str,
        /// The offending argument.
        argument: String,
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
    /// An environment addition has a name no process environment can carry.
    #[error("environment variable name {0:?} is empty or contains '=' or NUL")]
    InvalidEnvName(String),
    /// A limit is zero.
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
    /// The session has finished and accepts no more control.
    #[error("the session has finished")]
    SessionClosed,
}
