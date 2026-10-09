//! The CLI exit-code contract.
//!
//! Scripts and CI drive `basemind` headlessly, so the process status is part of the interface:
//!
//! | code | meaning |
//! |------|---------|
//! | `0`   | success |
//! | `1`   | runtime error (I/O, scan failure, a tool reported an error) |
//! | `2`   | usage error: a bad flag (clap) or a bad argument the command rejected before doing any work (a path outside the repo, a destructive command run without `--yes`) |
//! | `3`   | busy: another process holds the workspace writer lock (retry later, or route through the daemon) |
//! | `4`   | unavailable: the operation needs the comms daemon and none is reachable |
//! | `130` | interrupted (Ctrl-C); work completed before the interrupt is committed, nothing is torn |
//!
//! `3` is split from `1` because it is the one failure a caller should retry rather than report;
//! `4` is split because the remedy (start the daemon) differs from every other error. `130` is the
//! shell convention `128 + SIGINT`.

use std::fmt;

/// Success.
pub const OK: u8 = 0;
/// Runtime error.
pub const ERROR: u8 = 1;
/// Usage error.
pub const USAGE: u8 = 2;
/// Workspace writer lock held elsewhere.
pub const BUSY: u8 = 3;
/// A required daemon is unreachable.
pub const UNAVAILABLE: u8 = 4;
/// Interrupted by Ctrl-C.
pub const INTERRUPTED: u8 = 130;

/// An error that carries its own exit status. Wrap it in [`anyhow::Error`]; [`exit_code_for`]
/// finds it anywhere in the context chain.
#[derive(Debug)]
pub struct CliExit {
    pub code: u8,
    pub message: String,
}

impl CliExit {
    pub fn with_code(code: u8, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            code,
            message: message.into(),
        })
    }

    /// Exit `2`: the arguments were rejected before any work started.
    pub fn usage(message: impl Into<String>) -> anyhow::Error {
        Self::with_code(USAGE, message)
    }

    /// Exit `3`: the workspace writer lock is held by another process.
    pub fn busy(message: impl Into<String>) -> anyhow::Error {
        Self::with_code(BUSY, message)
    }

    /// Exit `4`: the daemon this operation needs cannot be reached.
    pub fn unavailable(message: impl Into<String>) -> anyhow::Error {
        Self::with_code(UNAVAILABLE, message)
    }

    /// Exit `130`: the user interrupted the run.
    pub fn interrupted(message: impl Into<String>) -> anyhow::Error {
        Self::with_code(INTERRUPTED, message)
    }
}

impl fmt::Display for CliExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliExit {}

/// The exit status for a failed command: the first [`CliExit`] in the chain, else `3` for a store
/// lock collision (however deep it is wrapped), else `1`.
pub fn exit_code_for(error: &anyhow::Error) -> u8 {
    for cause in error.chain() {
        if let Some(exit) = cause.downcast_ref::<CliExit>() {
            return exit.code;
        }
        if let Some(store) = cause.downcast_ref::<crate::store::StoreError>()
            && store.is_lock_contention()
        {
            return BUSY;
        }
    }
    ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_find_the_code_through_context_layers() {
        let error = CliExit::busy("held").context("open store").context("scan");
        assert_eq!(exit_code_for(&error), BUSY);
    }

    #[test]
    fn should_default_plain_errors_to_one() {
        assert_eq!(exit_code_for(&anyhow::anyhow!("boom")), ERROR);
    }

    #[test]
    fn should_map_each_named_constructor_to_its_code() {
        assert_eq!(exit_code_for(&CliExit::usage("u")), USAGE);
        assert_eq!(exit_code_for(&CliExit::unavailable("d")), UNAVAILABLE);
        assert_eq!(exit_code_for(&CliExit::interrupted("i")), INTERRUPTED);
    }
}
