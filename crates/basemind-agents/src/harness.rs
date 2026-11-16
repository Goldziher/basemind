// The bypass mode names are taken from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{agent.rs,provider/acp.rs}) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! Which harness to start and how to find it.

use std::path::PathBuf;

/// The dialect an ACP agent needs beyond the protocol itself: its launch arguments and the name
/// of its permission-bypass mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AcpFlavor {
    /// Cursor's `cursor-agent acp`.
    Cursor,
    /// Gemini CLI's ACP mode.
    Gemini,
    /// Grok Build's `grok agent stdio`.
    Grok,
    /// Hermes Agent's `hermes acp`.
    Hermes,
    /// OpenCode's `opencode acp`.
    OpenCode,
    /// Any other ACP agent, started from its executable without arguments: one that needs
    /// some is launched through a wrapper executable.
    Generic,
}

/// The harness family, which selects the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HarnessKind {
    /// Claude Code, driven over its `stream-json` input and output.
    ClaudeCode,
    /// Codex, driven over `codex app-server`.
    Codex,
    /// An agent that speaks the Agent Client Protocol on stdio.
    Acp(AcpFlavor),
}

impl HarnessKind {
    /// A stable lower-case id, also the default `BASEMIND_HOST` of a launched agent.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::Acp(AcpFlavor::Cursor) => "cursor",
            Self::Acp(AcpFlavor::Gemini) => "gemini",
            Self::Acp(AcpFlavor::Grok) => "grok",
            Self::Acp(AcpFlavor::Hermes) => "hermes",
            Self::Acp(AcpFlavor::OpenCode) => "opencode",
            Self::Acp(AcpFlavor::Generic) => "acp",
        }
    }

    /// The executable name looked up on `PATH` when the config names none.
    #[must_use]
    pub fn default_executable(self) -> &'static str {
        match self {
            Self::Acp(AcpFlavor::Cursor) => "cursor-agent",
            other => other.id(),
        }
    }

    /// The name of this harness's permission-bypass mode, which a
    /// [`BypassOptIn`](crate::BypassOptIn) has to repeat. `None` when the harness has no fixed
    /// one: Cursor and OpenCode offer none here, and a generic ACP agent takes the session mode
    /// id from the opt-in itself.
    #[must_use]
    pub fn bypass_mode(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => Some("bypassPermissions"),
            Self::Codex => Some("danger-full-access"),
            Self::Acp(AcpFlavor::Grok) => Some("yoloMode"),
            Self::Acp(AcpFlavor::Gemini) => Some("yolo"),
            Self::Acp(AcpFlavor::Hermes) => Some("dont_ask"),
            Self::Acp(AcpFlavor::Cursor | AcpFlavor::OpenCode | AcpFlavor::Generic) => None,
        }
    }
}

impl std::fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// One harness installation: its kind and its executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Harness {
    /// The harness family.
    pub kind: HarnessKind,
    /// The executable to run: an absolute path, or a bare name looked up on the absolute
    /// entries of the launcher's `PATH` (on Windows, with its `PATHEXT` extensions) when the
    /// launch is planned. A relative path with a
    /// directory part, or an executable inside the root, is refused with
    /// [`LaunchError::InvalidExecutable`](crate::LaunchError::InvalidExecutable). The
    /// harness's own lookups (an interpreter, its helpers) use the child's `PATH`.
    pub executable: PathBuf,
}

impl Harness {
    /// A harness of `kind` run from its default executable name.
    #[must_use]
    pub fn new(kind: HarnessKind) -> Self {
        Self {
            kind,
            executable: PathBuf::from(kind.default_executable()),
        }
    }

    /// The same harness run from `executable`.
    #[must_use]
    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = executable.into();
        self
    }
}
