//! The `basemind ui` re-exec shim.
//!
//! The desktop UI is a separate binary (`basemind-ui`). The root crate cannot depend on it — that
//! would cycle (`basemind-ui` depends on `basemind`), and the UI crate will later carry a per-
//! platform webview toolchain that must stay out of the default build. So `basemind ui` locates the
//! sibling `basemind-ui` shipped alongside `basemind` in the release archive (falling back to
//! `PATH`) and re-execs it, forwarding args verbatim and inheriting the environment. This mirrors
//! [`crate::agent_cmd`], which launches `basemind-tui` the same way.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Basename of the sibling desktop-UI binary this shim launches. Windows adds `.exe`.
#[cfg(windows)]
const UI_BINARY: &str = "basemind-ui.exe";
#[cfg(not(windows))]
const UI_BINARY: &str = "basemind-ui";

/// Locate the `basemind-ui` binary as a sibling of the running executable, returning `Some(path)`
/// only when that sibling file actually exists. Pure and side-effect-free (it never execs), so the
/// resolution logic is unit-testable without replacing the test process.
fn resolve_ui_binary(current_exe: &std::path::Path) -> Option<PathBuf> {
    let sibling = current_exe.parent()?.join(UI_BINARY);
    sibling.is_file().then_some(sibling)
}

/// Launch the `basemind-ui` desktop UI, forwarding `args` verbatim and inheriting the environment.
///
/// It prefers the sibling binary shipped alongside `basemind` in the release archive and falls back
/// to resolving the bare `basemind-ui` name against `PATH`. When the caller did not forward a
/// `--root`, the top-level `--root` is injected so the UI targets the same repository this
/// invocation selected.
pub fn run(root: &std::path::Path, args: &[String]) -> Result<()> {
    let current_exe = std::env::current_exe().context("locate the running basemind executable")?;
    let program: std::ffi::OsString =
        resolve_ui_binary(&current_exe).map_or_else(|| UI_BINARY.into(), PathBuf::into_os_string);

    let mut child_args: Vec<String> = Vec::with_capacity(args.len() + 2);
    // Only inject --root when the caller did not already forward one through.  ~keep
    if !args.iter().any(|arg| arg == "--root") {
        child_args.push("--root".to_string());
        child_args.push(root.to_string_lossy().into_owned());
    }
    child_args.extend_from_slice(args);

    launch_ui(&program, &child_args)
}

/// Map an OS launch failure for the UI binary into actionable guidance. A `NotFound` means the
/// binary is missing next to `basemind` and off `PATH`; anything else is wrapped with the program
/// path for context.
fn ui_launch_error(program: &std::ffi::OsStr, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "the basemind desktop-UI binary ({UI_BINARY}) was not found next to `basemind` \
             or on PATH; it ships in the release archive alongside `basemind`"
        )
    } else {
        anyhow::Error::new(error).context(format!("launch {}", program.to_string_lossy()))
    }
}

/// Replace this process with the UI binary. `exec` inherits env + stdio and only returns on
/// failure, so a return is always an error.
#[cfg(unix)]
fn launch_ui(program: &std::ffi::OsStr, args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(program).args(args).exec();
    Err(ui_launch_error(program, error))
}

/// Non-unix fallback: spawn the UI binary, wait, and propagate its exit code (no `exec`).
#[cfg(not(unix))]
fn launch_ui(program: &std::ffi::OsStr, args: &[String]) -> Result<()> {
    match std::process::Command::new(program).args(args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => Err(ui_launch_error(program, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{UI_BINARY, resolve_ui_binary};

    #[test]
    fn resolves_sibling_binary_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let current_exe = dir.path().join("basemind");
        std::fs::write(&current_exe, b"").expect("write fake basemind");
        let sibling = dir.path().join(UI_BINARY);
        std::fs::write(&sibling, b"").expect("write fake basemind-ui");

        assert_eq!(resolve_ui_binary(&current_exe), Some(sibling));
    }

    #[test]
    fn returns_none_when_sibling_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let current_exe = dir.path().join("basemind");
        std::fs::write(&current_exe, b"").expect("write fake basemind");

        assert_eq!(resolve_ui_binary(&current_exe), None);
    }
}
