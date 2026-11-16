//! The adapters a launcher knows, looked up by harness kind.

use crate::acp::AcpAdapter;
use crate::claude::ClaudeCodeAdapter;
use crate::codex::CodexAdapter;
use crate::error::LaunchError;
use crate::harness::HarnessKind;
use crate::session::{CommandPlan, Session};
use crate::spec::{LaunchConfig, LaunchRequest};

/// Turns a [`LaunchConfig`] and a [`LaunchRequest`] into a running [`Session`] for the
/// harnesses it supports.
pub trait Adapter: Send + Sync {
    /// Whether this adapter drives harnesses of `kind`.
    fn supports(&self, kind: HarnessKind) -> bool;

    /// The process a launch of `request` under `config` would start, without starting it.
    ///
    /// # Errors
    ///
    /// Whatever makes the launch unsafe or impossible: see [`LaunchError`].
    fn plan(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<CommandPlan, LaunchError>;

    /// Starts the harness and hands it the brief.
    ///
    /// # Errors
    ///
    /// The errors of [`Adapter::plan`], and [`LaunchError::Spawn`] when the executable does
    /// not start.
    fn launch(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<Session, LaunchError>;
}

/// A set of adapters. A later registration wins over an earlier one for the same kind.
#[derive(Default)]
pub struct Registry {
    adapters: Vec<Box<dyn Adapter>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("adapters", &self.adapters.len())
            .finish()
    }
}

impl Registry {
    /// A registry without adapters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the Claude Code, Codex and ACP adapters.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(ClaudeCodeAdapter);
        registry.register(CodexAdapter);
        registry.register(AcpAdapter);
        registry
    }

    /// Adds `adapter`, ahead of the ones already registered.
    pub fn register(&mut self, adapter: impl Adapter + 'static) {
        self.adapters.push(Box::new(adapter));
    }

    /// The adapter for `kind`.
    #[must_use]
    pub fn adapter(&self, kind: HarnessKind) -> Option<&dyn Adapter> {
        self.adapters
            .iter()
            .rev()
            .find(|adapter| adapter.supports(kind))
            .map(AsRef::as_ref)
    }

    /// The process a launch of `request` under `config` would start.
    ///
    /// # Errors
    ///
    /// [`LaunchError::UnknownHarness`] without an adapter for the config's harness, else the
    /// errors of [`Adapter::plan`].
    pub fn plan(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<CommandPlan, LaunchError> {
        self.require(config)?.plan(config, request)
    }

    /// Launches `request` under `config` with the adapter for the config's harness.
    ///
    /// # Errors
    ///
    /// [`LaunchError::UnknownHarness`] without an adapter for the config's harness, else the
    /// errors of [`Adapter::launch`].
    pub fn launch(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<Session, LaunchError> {
        self.require(config)?.launch(config, request)
    }

    fn require(&self, config: &LaunchConfig) -> Result<&dyn Adapter, LaunchError> {
        let kind = config.harness.kind;
        self.adapter(kind).ok_or(LaunchError::UnknownHarness(kind.id()))
    }
}
