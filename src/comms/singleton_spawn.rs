//! Resolving the executable to re-exec as the daemon, and spawning it detached.
//!
//! A child of [`singleton`](super) rather than part of it because the question it answers is
//! self-contained and unusually dangerous: *which* binary may be re-exec'd. Blindly re-exec'ing
//! `current_exe()` is a fork bomb rather than a mistake — under `cargo test` that path is the
//! libtest harness, which reads the appended `comms daemon` as a test-name filter and re-runs the
//! whole suite, every generation spawning the next. Keeping the refusal and the spawn together
//! means the guard cannot be bypassed by a caller that reaches for the spawn directly.

use std::path::{Path, PathBuf};

use super::CommsPaths;

/// Env override naming the `basemind` binary to spawn the daemon from. Set it when the running
/// executable is not itself `basemind` — a test harness, or an embedding host.
pub const DAEMON_BINARY_ENV: &str = "BASEMIND_DAEMON_BINARY";

/// Resolve the executable to re-exec as `basemind comms daemon`.
///
/// This deliberately refuses to re-exec an executable that is not a `basemind` binary, because
/// blindly re-exec'ing `current_exe()` is a fork bomb rather than a mistake. Under `cargo test`,
/// `current_exe()` is the libtest harness: it reads the appended `comms daemon` as a *test-name
/// filter*, re-runs the whole suite, and every test in that re-run that reaches this function spawns
/// another generation. One stray call took this machine from ~800 to 8256 processes.
///
/// [`DAEMON_BINARY_ENV`] is the escape hatch for a legitimate non-`basemind` caller, and it is what
/// a test should set (to `env!("CARGO_BIN_EXE_basemind")`) when it genuinely wants a real daemon.
fn resolve_daemon_binary() -> std::io::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os(DAEMON_BINARY_ENV) {
        return Ok(PathBuf::from(explicit));
    }
    daemon_binary_for(&std::env::current_exe()?)
}

/// The [`resolve_daemon_binary`] decision for a given running executable, split out so the refusal
/// can be tested without a harness whose own path decides the outcome.
fn daemon_binary_for(exe: &Path) -> std::io::Result<PathBuf> {
    let stem = exe.file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or_default();
    if stem == "basemind" {
        return Ok(exe.to_path_buf());
    }
    // A cargo test harness lives at `target/<profile>/deps/<name>-<hash>`, with the real binary its
    // grandparent's `basemind`. Preferring that keeps a test that genuinely wants a daemon working
    // while still never re-exec'ing the harness itself.
    if let Some(sibling) = exe
        .parent()
        .and_then(std::path::Path::parent)
        .map(|dir| dir.join(if cfg!(windows) { "basemind.exe" } else { "basemind" }))
        .filter(|candidate| candidate.is_file())
    {
        return Ok(sibling);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "refusing to spawn a comms daemon by re-exec'ing {}: it is not a `basemind` binary. \
             Re-exec'ing a test harness makes it re-run its own suite and spawn again, without \
             bound. Set {DAEMON_BINARY_ENV} to a real `basemind` executable to spawn one from here.",
            exe.display()
        ),
    ))
}

/// Spawn `basemind comms daemon` detached so it outlives the spawning process. stdout/stderr
/// are redirected to null; the daemon's own tracing goes to its log sink.
pub fn spawn_detached_daemon(_paths: &CommsPaths) -> std::io::Result<()> {
    let exe = resolve_daemon_binary()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("comms")
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` takes no arguments and only detaches the child into a new session.
        unsafe {
            command.pre_exec(|| {
                let _ = detach_session();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CreateProcess` flag: the child has no controlling console.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        /// `CreateProcess` flag: the child starts a new process group (Ctrl-C/Break isolation).
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        /// `CreateProcess` flag: do not allocate a console window for the child.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        clear_std_handle_inheritance();
    }
    command.spawn()?;
    Ok(())
}

/// Clear `HANDLE_FLAG_INHERIT` on this process's standard input/output/error handles so a
/// subsequently spawned child does not inherit them. See the call site in [`spawn_detached_daemon`]
/// for why the detached daemon must not inherit a captured stdout/stderr pipe. Clearing the inherit
/// bit does not affect this process's own use of the handles — only whether children receive a
/// duplicate — so it is safe for the short-lived `comms start` CLI and for `serve` (whose later
/// child spawns pass their stdio explicitly rather than relying on inheritance).
#[cfg(windows)]
fn clear_std_handle_inheritance() {
    /// `GetStdHandle` selectors (Win32 `STD_*_HANDLE`, defined as negative `DWORD`s).
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    /// `SetHandleInformation` mask bit controlling handle inheritance.
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    /// `GetStdHandle` failure sentinel.
    const INVALID_HANDLE_VALUE: isize = -1;

    for selector in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: both calls take only primitive arguments and read no caller memory.
        unsafe {
            let handle = GetStdHandle(selector);
            if handle != 0 && handle != INVALID_HANDLE_VALUE {
                let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    /// Win32 `GetStdHandle` — returns the handle for a standard device (`STD_*_HANDLE`).
    fn GetStdHandle(nstdhandle: u32) -> isize;
    /// Win32 `SetHandleInformation` — sets the masked flag bits on a handle. Returns nonzero on
    /// success.
    fn SetHandleInformation(hobject: isize, dwmask: u32, dwflags: u32) -> i32;
}

#[cfg(unix)]
fn detach_session() -> std::io::Result<()> {
    // SAFETY: `setsid()` takes no arguments, reads no caller memory, and creates a new session
    let rc = unsafe { setsid() };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
unsafe extern "C" {
    /// POSIX `setsid(2)`.
    fn setsid() -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard against the fork bomb. `current_exe()` under `cargo test` IS this harness binary,
    /// and re-exec'ing it with `comms daemon` appended makes libtest read those as a test-name filter
    /// and re-run the whole suite — which spawns again, without bound. So the resolver must refuse
    /// here rather than hand back a path.
    #[test]
    fn should_refuse_to_re_exec_a_binary_that_is_not_basemind() {
        let harness = std::path::Path::new("/nowhere/target/debug/deps/git_history_daemon-17c2bf1b");
        let error = daemon_binary_for(harness).expect_err("a test harness must never be re-exec'd");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(
            message.contains("not a `basemind` binary") && message.contains(DAEMON_BINARY_ENV),
            "the refusal must name the cause and the escape hatch; got: {message}"
        );
    }

    #[test]
    fn should_re_exec_a_real_basemind_binary() {
        let real = std::path::Path::new("/usr/local/bin/basemind");
        assert_eq!(daemon_binary_for(real).expect("a basemind binary resolves"), real);
    }

    /// A harness DOES get a daemon when the real binary is sitting where cargo puts it — the guard
    /// redirects the spawn, it does not disable it. What it must never return is the harness itself.
    #[test]
    fn should_redirect_a_harness_to_the_sibling_basemind_instead_of_itself() {
        let target = tempfile::tempdir().expect("tempdir");
        let deps = target.path().join("deps");
        std::fs::create_dir_all(&deps).expect("create deps dir");
        let real = target
            .path()
            .join(if cfg!(windows) { "basemind.exe" } else { "basemind" });
        std::fs::write(&real, b"").expect("write the sibling binary");
        let harness = deps.join("comms_smoke-deadbeef");

        assert_eq!(
            daemon_binary_for(&harness).expect("the sibling resolves"),
            real,
            "the harness must be redirected to the real binary, never re-exec'd"
        );
    }

    /// The escape hatch for an embedding host whose executable is neither `basemind` nor sitting
    /// beside one.
    #[test]
    fn should_use_the_explicit_binary_override_when_set() {
        let previous = std::env::var_os(DAEMON_BINARY_ENV);
        let explicit = std::path::PathBuf::from("/opt/somewhere/basemind-wrapper");
        // SAFETY: single-threaded within this test; the previous value is restored before asserting.
        unsafe { std::env::set_var(DAEMON_BINARY_ENV, &explicit) };
        let resolved = resolve_daemon_binary();
        match previous {
            Some(value) => unsafe { std::env::set_var(DAEMON_BINARY_ENV, value) },
            None => unsafe { std::env::remove_var(DAEMON_BINARY_ENV) },
        }

        assert_eq!(resolved.expect("the override resolves"), explicit);
    }
}
