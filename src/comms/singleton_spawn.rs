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

/// Make the resolved binary path independent of the working directory, because
/// [`spawn_detached_daemon`] sets one. A relative path with a directory component (`target/debug/
/// basemind`, the shape [`DAEMON_BINARY_ENV`] most often takes in a test) is resolved by the child
/// *after* its `chdir`, so it would silently stop existing. A bare program name has no directory
/// component and is looked up on `PATH`, which the cwd does not affect — left alone.
fn absolute_daemon_binary(exe: PathBuf) -> std::io::Result<PathBuf> {
    if exe.is_absolute() || exe.parent().is_none_or(|parent| parent.as_os_str().is_empty()) {
        return Ok(exe);
    }
    Ok(std::env::current_dir()?.join(exe))
}

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

/// The spawning agent's identity, inherited by every child unless removed. A daemon that inherited
/// them would answer as whichever agent happened to start it — and, worse, would keep answering as
/// that agent long after the session was gone. The daemon resolves no identity of its own (it is a
/// broker, not a participant), so removing them takes nothing away from it. A launched agent's depth
/// and launch token (`basemind-agents`) are among them: a daemon a launched agent's basemind call
/// (re)spawned would otherwise keep a live token in a long-lived process's environment.
const INHERITED_IDENTITY_VARS: [&str; 5] = [
    "BASEMIND_AGENT_ID",
    "BASEMIND_PARENT_AGENT_ID",
    "BASEMIND_THREAD_ID",
    "BASEMIND_AGENT_DEPTH",
    "BASEMIND_LAUNCH_TOKEN",
];

/// Shell-maintained records of the *spawning* shell's directory. They are not the child's cwd — the
/// shell writes them, nothing keeps them in step with `chdir` — so leaving them set hands the daemon
/// a second, contradictory answer to "where am I", pointing at a repository it has no relationship
/// with.
const INHERITED_CWD_VARS: [&str; 2] = ["PWD", "OLDPWD"];

/// Grants that widen what the daemon does for every session on the machine: admit any root or extra
/// roots, read dependency package roots outside the repository, follow symlinks, index credential
/// files, open the HTTP transport and where it binds. A daemon serves every session of the
/// generation, so one that inherited them from whichever shell happened to start it applied that
/// shell's grants to all of them. They reach the daemon only from an operator's explicit start
/// (`comms start`, or `daemon ensure` with the HTTP grant set).
const INHERITED_GRANT_VARS: [&str; 7] = [
    crate::config::root_guard::ALLOW_ANY_ROOT_ENV,
    crate::scanner_candidates::ALLOW_EXTRA_ROOTS_ENV,
    crate::config::trust::ALLOW_DEPENDENCY_ROOTS_ENV,
    crate::config::trust::ALLOW_FOLLOW_SYMLINKS_ENV,
    crate::scanner_filter::ALLOW_REPO_CREDENTIALS_ENV,
    crate::comms::http_auth::ALLOW_HTTP_ENV,
    crate::comms::http_frontend::HTTP_ADDR_ENV,
];

/// Whether a spawn passes the spawner's [`INHERITED_GRANT_VARS`] on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grants {
    /// An implicit spawn (a session, a CLI call): the grants are removed.
    Withheld,
    /// An operator's explicit start: the grants in its environment are the ones meant.
    Passed,
}

/// File name of the detached daemon's combined stdout/stderr log, inside the comms dir.
pub const DAEMON_LOG_FILE: &str = "daemon.log";
/// Size past which the log is rotated at spawn and trimmed while the daemon runs.
pub const DAEMON_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Where the daemon for `comms_dir` writes its log.
pub fn daemon_log_path(comms_dir: &Path) -> PathBuf {
    comms_dir.join(DAEMON_LOG_FILE)
}

/// Open the daemon log for append, first rotating an oversized one to `daemon.log.1` (replacing any
/// previous rotation). `None` when it cannot be opened — the daemon then runs with null stdio rather
/// than failing to start over a log file.
fn open_daemon_log(comms_dir: &Path) -> Option<std::fs::File> {
    let path = daemon_log_path(comms_dir);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > DAEMON_LOG_MAX_BYTES) {
        let _ = std::fs::rename(&path, comms_dir.join(format!("{DAEMON_LOG_FILE}.1")));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).ok()
}

/// Keep a long-running daemon's log bounded: once it passes [`DAEMON_LOG_MAX_BYTES`], preserve the
/// content as `daemon.log.1` and truncate in place. The daemon holds the file open in append mode,
/// so truncating (rather than renaming) keeps its stdout/stderr pointed at the live file.
pub fn trim_daemon_log(comms_dir: &Path) {
    let path = daemon_log_path(comms_dir);
    if !std::fs::metadata(&path).is_ok_and(|meta| meta.len() > DAEMON_LOG_MAX_BYTES) {
        return;
    }
    if std::fs::copy(&path, comms_dir.join(format!("{DAEMON_LOG_FILE}.1"))).is_ok() {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_len(0));
    }
}

/// The `basemind comms daemon` invocation, minus the platform detach flags: argv, working
/// directory, stdio and the environment scrub. Split out so the hygiene above is assertable without
/// launching a process — the properties that matter here are all visible on the [`Command`] itself.
///
/// [`Command`]: std::process::Command
fn daemon_command(exe: &Path, paths: &CommsPaths, grants: Grants) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .arg("comms")
        .arg("daemon")
        .current_dir(&paths.comms_dir)
        .stdin(std::process::Stdio::null());
    match open_daemon_log(&paths.comms_dir).and_then(|log| log.try_clone().ok().map(|dup| (log, dup))) {
        Some((out, err)) => {
            command.stdout(out).stderr(err);
        }
        None => {
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
    }
    // Targeted removals, never `env_clear`: the daemon needs PATH, HOME, the XDG dirs, the proxy
    // variables and the model configuration to function at all.
    for key in INHERITED_IDENTITY_VARS.iter().chain(INHERITED_CWD_VARS.iter()) {
        command.env_remove(key);
    }
    if grants == Grants::Withheld {
        for key in INHERITED_GRANT_VARS {
            command.env_remove(key);
        }
    }
    // The daemon serves this process's data home. An embedder sets it through `Engine::new`, not
    // the environment, so an inherited variable may be unset or name another home.
    command.env(
        crate::store::DATA_HOME_ENV,
        crate::data_root::DataRoot::current().path(),
    );
    command
}

/// Spawn `basemind comms daemon` detached so it outlives the spawning process. stdout/stderr
/// are appended to `<comms_dir>/daemon.log` (size-rotated), falling back to null if it cannot be opened.
///
/// The child is placed in `paths.comms_dir` rather than inheriting the spawner's cwd. The comms dir
/// is created by `resolve_paths`, is stable for the daemon's whole life, and — decisively — is never
/// a repository. An inherited cwd is none of those: a daemon spawned from a shell sitting at `/`
/// inherits `/`, and repository-root discovery from `/` is how issue #62 produced a workspace root
/// of `/` and a scan of the entire filesystem. Nothing in the daemon reads its own cwd, so this
/// costs nothing and removes the whole class.
pub fn spawn_detached_daemon(paths: &CommsPaths) -> std::io::Result<()> {
    spawn_detached_daemon_with(paths, Grants::Withheld)
}

/// [`spawn_detached_daemon`] choosing whether the spawner's grants reach the daemon.
pub fn spawn_detached_daemon_with(paths: &CommsPaths, grants: Grants) -> std::io::Result<()> {
    let exe = absolute_daemon_binary(resolve_daemon_binary()?)?;
    // `resolve_paths` already made it; recreate defensively because `current_dir` on a missing
    // directory fails the spawn outright, and losing the daemon to a tidied cache dir is worse
    // than the cwd hygiene is good.
    let _ = std::fs::create_dir_all(&paths.comms_dir);
    spawn_detached(daemon_command(&exe, paths, grants))
}

/// Spawn `command` so that it outlives this process and is never this process's child once it runs.
///
/// Unix: the forked child starts a new session, forks again and exits; the grandchild execs.
/// `spawn` returns once the grandchild has exec'd (or reports its exec error), and the
/// intermediate is waited on at once, so it leaves no zombie. The daemon is then a child of init
/// (or the nearest subreaper), which reaps it. Before this, `serve` dropped the `Child` and never
/// waited, so every daemon it spawned stayed a zombie of `serve` for its whole life once it
/// exited, and `kill(pid, 0)` read that zombie as a live daemon.
///
/// Windows has no zombies: dropping the `Child` closes its handle and the exited process goes.
fn spawn_detached(command: std::process::Command) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut command = command;
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs in the forked child before exec and calls only `setsid`,
        // `fork` and `_exit`, all async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                let _ = detach_session();
                match libc::fork() {
                    -1 => Err(std::io::Error::last_os_error()),
                    0 => Ok(()),
                    _ => libc::_exit(0),
                }
            });
        }
        let mut intermediate = command.spawn()?;
        intermediate.wait()?;
        Ok(())
    }
    #[cfg(windows)]
    {
        spawn_windows(command)
    }
}

#[cfg(windows)]
fn spawn_windows(mut command: std::process::Command) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    /// `CreateProcess` flag: the child has no controlling console.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    /// `CreateProcess` flag: the child starts a new process group (Ctrl-C/Break isolation).
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    /// `CreateProcess` flag: do not allocate a console window for the child.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    clear_std_handle_inheritance();
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

    fn paths_at(comms_dir: &std::path::Path) -> CommsPaths {
        CommsPaths {
            comms_dir: comms_dir.to_path_buf(),
            socket_path: comms_dir.join("comms.sock"),
        }
    }

    #[test]
    fn oversized_daemon_log_is_rotated_at_spawn_and_trimmed_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = daemon_log_path(dir.path());
        let big = vec![b'x'; usize::try_from(DAEMON_LOG_MAX_BYTES).expect("fits") + 1];

        std::fs::write(&log, &big).expect("seed");
        drop(open_daemon_log(dir.path()).expect("open"));
        assert_eq!(
            std::fs::metadata(&log).expect("fresh log").len(),
            0,
            "spawn starts a fresh log"
        );
        assert!(dir.path().join("daemon.log.1").exists(), "the old log is kept as .1");

        std::fs::write(&log, &big).expect("grow");
        trim_daemon_log(dir.path());
        assert_eq!(std::fs::metadata(&log).expect("trimmed").len(), 0);
        assert!(std::fs::metadata(dir.path().join("daemon.log.1")).expect(".1").len() > DAEMON_LOG_MAX_BYTES);
    }

    /// Issue #62's root cause in one assertion: a daemon that inherits the spawner's cwd inherits
    /// whatever repository — or `/` — that shell happened to be sitting in.
    #[test]
    fn the_daemon_runs_in_the_comms_dir_not_the_spawner_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let command = daemon_command(
            std::path::Path::new("/usr/local/bin/basemind"),
            &paths_at(dir.path()),
            Grants::Withheld,
        );

        assert_eq!(
            command.get_current_dir(),
            Some(dir.path()),
            "the child must be placed in the comms dir, which is never a repository"
        );
    }

    /// The scrub is targeted, and the negative half is the load-bearing one: `env_clear` would take
    /// PATH, HOME and the proxy/model configuration with it and leave a daemon that cannot work.
    #[test]
    fn the_child_environment_drops_identity_and_cwd_vars_and_keeps_everything_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let command = daemon_command(
            std::path::Path::new("/usr/local/bin/basemind"),
            &paths_at(dir.path()),
            Grants::Withheld,
        );

        let removed: Vec<&std::ffi::OsStr> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key)
            .collect();
        for key in INHERITED_IDENTITY_VARS.iter().chain(INHERITED_CWD_VARS.iter()) {
            assert!(
                removed.contains(&std::ffi::OsStr::new(key)),
                "{key} must not reach the daemon; removed: {removed:?}"
            );
        }
        let set: Vec<&std::ffi::OsStr> = command
            .get_envs()
            .filter(|(_, value)| value.is_some())
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            set,
            [std::ffi::OsStr::new(crate::store::DATA_HOME_ENV)],
            "only removals and the data home belong here — another override would mean the parent env \
             is being rebuilt"
        );
        assert_eq!(
            removed.len(),
            INHERITED_IDENTITY_VARS.len() + INHERITED_CWD_VARS.len() + INHERITED_GRANT_VARS.len(),
            "nothing the daemon needs (PATH, HOME, XDG, proxy, model vars) may be swept up: {removed:?}"
        );
    }

    /// The daemon gets the spawner's installed data home, whatever the inherited variable says.
    #[test]
    fn the_daemon_is_given_the_installed_data_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let command = daemon_command(
            std::path::Path::new("/usr/local/bin/basemind"),
            &paths_at(dir.path()),
            Grants::Withheld,
        );
        let home = command
            .get_envs()
            .find(|(key, _)| *key == crate::store::DATA_HOME_ENV)
            .and_then(|(_, value)| value);
        assert_eq!(home, Some(crate::data_root::DataRoot::current().path().as_os_str()));
    }

    /// A daemon spawned from inside a launched session carries neither its token nor its depth.
    #[test]
    fn a_launched_sessions_token_and_depth_never_reach_the_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let command = daemon_command(
            std::path::Path::new("/usr/local/bin/basemind"),
            &paths_at(dir.path()),
            Grants::Passed,
        );
        let removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for key in ["BASEMIND_LAUNCH_TOKEN", "BASEMIND_AGENT_DEPTH"] {
            assert!(removed.iter().any(|removed| removed == key), "{key}: {removed:?}");
        }
    }

    /// One shell with `BASEMIND_ALLOW_ANY_ROOT=1` or `BASEMIND_ALLOW_HTTP=1` that happened to start
    /// the daemon lifted the root guard, or opened TCP, for every session on the machine. The list is
    /// exact: a grant left out of it still reaches every session.
    #[test]
    fn an_implicit_spawn_withholds_the_grants_and_an_explicit_one_passes_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = std::path::Path::new("/usr/local/bin/basemind");
        let removed = |grants| {
            daemon_command(exe, &paths_at(dir.path()), grants)
                .get_envs()
                .filter(|(_, value)| value.is_none())
                .map(|(key, _)| key.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let implicit = removed(Grants::Withheld);
        let explicit = removed(Grants::Passed);
        let mut withheld: Vec<&str> = implicit
            .iter()
            .filter(|key| !explicit.contains(*key))
            .map(String::as_str)
            .collect();
        withheld.sort_unstable();
        assert_eq!(
            withheld,
            [
                "BASEMIND_ALLOW_ANY_ROOT",
                "BASEMIND_ALLOW_DEPENDENCY_ROOTS",
                "BASEMIND_ALLOW_EXTRA_ROOTS",
                "BASEMIND_ALLOW_FOLLOW_SYMLINKS",
                "BASEMIND_ALLOW_HTTP",
                "BASEMIND_ALLOW_REPO_CREDENTIALS",
                "BASEMIND_HTTP_ADDR",
            ]
        );
        assert!(
            INHERITED_GRANT_VARS
                .iter()
                .all(|key| !explicit.iter().any(|removed| removed == key)),
            "{explicit:?}"
        );
    }

    /// Setting a working directory changes how the child resolves a relative program path, so the
    /// binary must be pinned first. `BASEMIND_DAEMON_BINARY=target/debug/basemind` is the common
    /// shape, and it would resolve against the comms dir and vanish.
    #[test]
    fn a_relative_binary_is_absolutised_before_the_working_directory_changes() {
        let relative = PathBuf::from("target/debug/basemind");
        let resolved = absolute_daemon_binary(relative.clone()).expect("absolutised");
        assert!(resolved.is_absolute(), "got {}", resolved.display());
        assert!(resolved.ends_with(&relative));

        let absolute = PathBuf::from(if cfg!(windows) {
            r"C:\bin\basemind.exe"
        } else {
            "/usr/local/bin/basemind"
        });
        assert_eq!(absolute_daemon_binary(absolute.clone()).expect("unchanged"), absolute);

        // A bare name is a PATH lookup, which the working directory does not affect.
        let bare = PathBuf::from("basemind");
        assert_eq!(absolute_daemon_binary(bare.clone()).expect("unchanged"), bare);
    }

    /// A spawned daemon must never be this process's child: a `serve` that lives for hours would
    /// keep one zombie per daemon it ever spawned, each read as a live daemon. Spawn, kill, and
    /// observe that the process is not ours to reap and is gone, not defunct.
    #[cfg(unix)]
    #[test]
    fn a_detached_spawn_leaves_no_child_to_reap_and_no_zombie() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("pid");
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("echo $$ > '{}'; exec sleep 30", pidfile.display()))
            .stdin(std::process::Stdio::null());
        spawn_detached(command).expect("spawn");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "the child never wrote its pid");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(
            crate::boot_identity::pid_state(pid as u32),
            crate::boot_identity::PidState::Live
        );

        // SAFETY: `pid` names the process just spawned, which runs `sleep`.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        // SAFETY: `WNOHANG` returns at once; a null status pointer is allowed.
        let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            (reaped, errno),
            (-1, Some(libc::ECHILD)),
            "the daemon must not be a child of the spawner"
        );
        while crate::boot_identity::pid_state(pid as u32) != crate::boot_identity::PidState::Gone {
            assert!(std::time::Instant::now() < deadline, "the killed process lingers");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
