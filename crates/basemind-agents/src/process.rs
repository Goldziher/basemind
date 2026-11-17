// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/process.rs) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! Process-group handling: a harness is started as the leader of its own group, so ending the
//! session ends every helper that stayed in that group. A descendant that leaves the group
//! (a daemonizing helper, a job started under job control) is not reached and survives the
//! session; containing it needs a cgroup or a Job Object, which this crate does not set up.

use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How long a harness whose stdin was closed may take to exit by itself.
pub(crate) const EXIT_GRACE: Duration = Duration::from_millis(1500);
/// How long the group leader may take to shut down after SIGTERM.
pub(crate) const TERM_GRACE: Duration = Duration::from_secs(2);

/// The error a spawn fails with when the working directory changed after it was checked.
#[cfg(unix)]
pub(crate) const CWD_CHANGED: i32 = libc::ESTALE;

/// Makes the child the leader of a new process group with the ordinary signal mask.
pub(crate) fn isolate(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs between fork and exec and only makes async-signal-safe
        // system calls: it neither allocates nor takes a lock.
        unsafe {
            command.pre_exec(|| {
                // exec preserves the signal mask. A launcher thread that blocks SIGCHLD would
                // hand that mask to the harness, whose own child processes then never finish.
                // Not redundant: without it `a_child_does_not_inherit_a_blocked_sigchld`
                // fails, so std does not reset the mask on every spawn path.
                let mut signals = std::mem::zeroed();
                libc::sigemptyset(&mut signals);
                // pthread_sigmask, as sigprocmask is unspecified in a multithreaded process;
                // it returns the error number instead of setting errno.
                let status = libc::pthread_sigmask(libc::SIG_SETMASK, &signals, std::ptr::null_mut());
                if status != 0 {
                    return Err(std::io::Error::from_raw_os_error(status));
                }
                Ok(())
            });
        }
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
}

/// Makes the spawn fail with [`CWD_CHANGED`] unless the child's working directory, after std
/// changed into it, is exactly `expected`: a directory swapped for a symlink between the
/// check and the spawn is caught here, in the child, before the harness runs.
#[cfg(unix)]
pub(crate) fn verify_cwd(command: &mut Command, expected: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let expected = expected.as_os_str().as_bytes().to_vec();
    // Allocated here, in the parent: the closure runs between fork and exec, where only
    // async-signal-safe calls are allowed.
    let mut buffer = vec![0_u8; libc::PATH_MAX as usize + 1];
    // SAFETY: the closure only calls getcwd and strlen on a buffer it owns and compares
    // bytes; it neither allocates nor takes a lock.
    unsafe {
        command.pre_exec(move || {
            let cwd = libc::getcwd(buffer.as_mut_ptr().cast(), buffer.len());
            if cwd.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let len = libc::strlen(cwd);
            if buffer[..len] != expected[..] {
                return Err(std::io::Error::from_raw_os_error(CWD_CHANGED));
            }
            Ok(())
        });
    }
}

/// The launcher's end of the pipe a harness's keeper waits on. Dropping it, or the launcher
/// dying, which closes it, makes the keeper kill the harness's whole process group.
#[cfg(unix)]
pub(crate) struct Keeper {
    /// Held, never written: its closing is the signal.
    _write: std::os::fd::OwnedFd,
    /// The keeper's end, open in the launcher only until the harness is spawned.
    read: Option<std::os::fd::OwnedFd>,
}

#[cfg(unix)]
impl Keeper {
    /// Closes the launcher's copy of the keeper's end once the spawn has forked.
    pub(crate) fn spawned(&mut self) {
        self.read = None;
    }
}

/// The most file descriptors a keeper closes by number where the platform cannot close a
/// range at once.
#[cfg(unix)]
const KEEPER_CLOSES: libc::rlim_t = 1 << 16;

/// Arranges for a keeper process in the harness's group that kills the group once the launcher
/// is gone, so a launcher killed outright (SIGKILL, a crash) leaves no harness behind.
///
/// Between fork and exec the child forks the keeper, which closes every descriptor but the
/// read end of a pipe whose only write end is the returned [`Keeper`], blocks reading it, and on
/// end of file sends SIGKILL to its own process group (the harness's, which it is a member of,
/// so the id cannot have been reused) and exits. `PR_SET_PDEATHSIG` is not used on Linux: it
/// fires when the thread that spawned the child exits, not the launcher, and signals only the
/// leader. macOS has no equivalent; the pipe works the same on both.
#[cfg(unix)]
pub(crate) fn keep_group(command: &mut Command) -> std::io::Result<Keeper> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let mut ends = [0; 2];
    // SAFETY: pipe writes two descriptors into the array it is given.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nothing else.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
    for fd in [&read, &write] {
        // SAFETY: fcntl on a descriptor this function owns. Another thread forking before this
        // runs could carry the write end into its child, which only delays the keeper.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit fills in the struct it is given.
    let open_max = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur.min(KEEPER_CLOSES)
    } else {
        KEEPER_CLOSES
    } as libc::c_int;
    let read_fd = read.as_raw_fd();
    // SAFETY: the closure runs between fork and exec and only makes async-signal-safe calls
    // (fork, dup2, close, read, kill, _exit); it neither allocates nor takes a lock.
    unsafe {
        command.pre_exec(move || {
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => {
                    // The keeper: holding any of the harness's pipes would keep them open past
                    // its exit, and holding the write end would keep it from ever waking.
                    libc::dup2(read_fd, 0);
                    #[cfg(target_os = "linux")]
                    let closed = libc::syscall(libc::SYS_close_range, 1, libc::c_uint::MAX, 0) == 0;
                    #[cfg(not(target_os = "linux"))]
                    let closed = false;
                    if !closed {
                        for fd in 1..open_max {
                            libc::close(fd);
                        }
                    }
                    let mut byte = 0_u8;
                    while libc::read(0, (&raw mut byte).cast(), 1) < 0
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                    {
                    }
                    libc::kill(0, libc::SIGKILL);
                    libc::_exit(0)
                }
                _ => Ok(()),
            }
        });
    }
    Ok(Keeper {
        _write: write,
        read: Some(read),
    })
}

/// Time since an arbitrary fixed point that keeps counting while the machine sleeps, for the
/// wall-time limit: `Instant` does not (`CLOCK_UPTIME_RAW` on macOS, `CLOCK_MONOTONIC` on Linux),
/// so a laptop that slept would extend every session by the time it slept. Linux reads
/// `CLOCK_BOOTTIME`, macOS `CLOCK_MONOTONIC`, which there counts sleep; elsewhere `None`.
pub(crate) fn wall_clock() -> Option<Duration> {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        #[cfg(target_vendor = "apple")]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
        #[cfg(not(target_vendor = "apple"))]
        const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime fills in the timespec it is given.
        if unsafe { libc::clock_gettime(CLOCK, &mut now) } == 0 {
            return Some(Duration::new(now.tv_sec as u64, now.tv_nsec as u32));
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    None
}

/// Whether the kernel reaps this process's children by itself: SIGCHLD is ignored or
/// `SA_NOCLDWAIT` is set. Then a leader can be reaped before its group is signalled, and the
/// group id may belong to someone else by the time it is, so no harness is started.
#[cfg(unix)]
pub(crate) fn children_reaped_automatically() -> bool {
    // SAFETY: sigaction with a null new action only reads the current one into `current`,
    // which is plain data and valid all-zero.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut current) == 0
            && (current.sa_sigaction == libc::SIG_IGN || current.sa_flags & libc::SA_NOCLDWAIT != 0)
    }
}

/// Refuses a launch on a platform whose process containment is not in place. On Windows a
/// launched harness's tree would be ended with `taskkill /T`, which misses descendants whose
/// parent already exited, and the stop has no grace period; a Job Object is the real fix and
/// is not implemented, so nothing is launched there.
pub(crate) fn platform_supported() -> Result<(), &'static str> {
    if cfg!(unix) {
        Ok(())
    } else {
        Err("launching a harness on this platform, which has no process-tree containment yet")
    }
}

/// The pids this process signalled, so a test can tell a signal that should not have been
/// sent from one that found nobody.
#[cfg(all(test, unix))]
pub(crate) static SIGNALLED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Sends `signal` to `target` (a negative target is a process group), recording it in tests.
#[cfg(unix)]
fn signal(target: libc::pid_t, signal: libc::c_int) {
    #[cfg(test)]
    SIGNALLED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(target.unsigned_abs());
    // SAFETY: kill takes plain integers.
    unsafe {
        libc::kill(target, signal);
    }
}

/// Kills every process of the group `pid` leads. The caller must know the leader has not
/// been reaped by the session. A leader reaped by someone else (a `waitpid(-1)` elsewhere in
/// the launcher) is not ours any more: its id, and so its group's, may already name another
/// process, so nothing is signalled.
pub(crate) fn kill_group(pid: u32) {
    #[cfg(unix)]
    if leader(pid) != Leader::Gone {
        signal(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // By full path: a `taskkill` found through PATH or the working directory could be
        // anything.
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let mut kill = Command::new(std::path::Path::new(&root).join("System32").join("taskkill.exe"));
        kill.args(["/PID", &pid.to_string(), "/T", "/F"]);
        kill.creation_flags(CREATE_NO_WINDOW);
        let _ = kill.status();
    }
}

/// Kills the group at once and reaps the leader, unless someone else reaped it already.
pub(crate) fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if leader(child.id()) == Leader::Gone {
        return;
    }
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Ends a harness whose work is done without cutting off a write it has in flight.
/// `reading` says whether its stdin was closed with every frame delivered: only then can it
/// see the end of its input and exit by itself, so only then does it get [`EXIT_GRACE`].
///
/// Claude Code rotates its OAuth refresh token on every refresh and saves the new one through
/// a helper process. Killing the group in that window loses the token and signs the account
/// out. So: wait for an exit on stdin EOF, then SIGTERM the leader only, and SIGKILL the group
/// last. The group is always killed before the leader is reaped, while its id is still ours.
///
/// The two waits ([`EXIT_GRACE`], then [`TERM_GRACE`]) together take at most `budget`, and
/// both end as soon as `kill_now` is set (the session was dropped).
pub(crate) fn retire(child: &mut Child, budget: Duration, kill_now: &AtomicBool, reading: bool) {
    #[cfg(unix)]
    {
        let pid = child.id();
        let started = Instant::now();
        let exit_grace = if reading {
            EXIT_GRACE.min(budget)
        } else {
            Duration::ZERO
        };
        let mut state = settle(pid, exit_grace, kill_now);
        if state == Leader::Running && !kill_now.load(Ordering::SeqCst) {
            // The leader is unreaped, so the id is ours.
            signal(pid as libc::pid_t, libc::SIGTERM);
            let left = budget.saturating_sub(started.elapsed());
            state = settle(pid, TERM_GRACE.min(left), kill_now);
        }
        if state == Leader::Gone {
            return;
        }
    }
    #[cfg(not(unix))]
    let _ = (budget, kill_now, reading);
    terminate(child);
}

/// Where a group leader stands, observed without reaping it.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leader {
    /// Still running.
    Running,
    /// Exited and not yet reaped: its id, and its group's, are still ours.
    Exited,
    /// Reaped by someone else: its id may name another process by now.
    Gone,
}

/// The leader `pid`'s state now.
#[cfg(unix)]
fn leader(pid: u32) -> Leader {
    settle(pid, Duration::ZERO, &AtomicBool::new(false))
}

/// The leader `pid`'s state once it leaves [`Leader::Running`], or after `within`.
#[cfg(unix)]
fn settle(pid: u32, within: Duration, abort: &AtomicBool) -> Leader {
    let deadline = Instant::now() + within;
    loop {
        // SAFETY: siginfo_t is plain data that waitid fills in, and all-zero is a valid value
        // of it. WNOWAIT leaves the child a zombie, so its pid stays reserved.
        let (status, reported) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let status = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            (status, info.si_pid())
        };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // ECHILD: the child was reaped behind the session's back, by an ignored SIGCHLD
            // (refused at launch) or a foreign `waitpid`.
            return Leader::Gone;
        }
        if reported != 0 {
            return Leader::Exited;
        }
        if Instant::now() >= deadline || abort.load(Ordering::SeqCst) {
            return Leader::Running;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    #[test]
    fn nothing_is_launched_on_windows() {
        assert!(super::platform_supported().is_err());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn spawn(script: &str) -> Child {
        let mut command = Command::new("/bin/sh");
        // The scripts start helpers that outlive them; none may hold the test's own output.
        command
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        isolate(&mut command);
        command.spawn().unwrap()
    }

    fn group_alive(pid: u32) -> bool {
        // SAFETY: signal 0 only probes whether the group has a member.
        unsafe { libc::kill(-(pid as libc::pid_t), 0) == 0 }
    }

    fn wait_for_group_exit(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while group_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_wall_clock_runs() {
        let before = wall_clock().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let after = wall_clock().unwrap();
        assert!(after >= before + Duration::from_millis(20), "{before:?} {after:?}");
    }

    #[test]
    fn unix_launches_are_supported() {
        assert_eq!(platform_supported(), Ok(()));
    }

    #[test]
    fn retire_lets_a_child_finish_on_stdin_eof_before_any_signal() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("saved");
        // Exits on EOF after a short write; a signal would skip the write.
        let mut child = spawn(&format!(
            "trap '' TERM; cat >/dev/null; sleep 0.2; echo saved > '{}'",
            marker.display()
        ));
        drop(child.stdin.take());
        let started = Instant::now();
        retire(&mut child, EXIT_GRACE + TERM_GRACE, &AtomicBool::new(false), true);
        assert!(started.elapsed() < EXIT_GRACE);
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "saved");
    }

    #[test]
    fn retire_escalates_when_a_child_ignores_stdin_and_sigterm() {
        let mut child = spawn("trap '' TERM; while :; do sleep 0.05; done");
        let pid = child.id();
        drop(child.stdin.take());
        let started = Instant::now();
        retire(&mut child, EXIT_GRACE + TERM_GRACE, &AtomicBool::new(false), true);
        assert!(started.elapsed() >= EXIT_GRACE + TERM_GRACE - Duration::from_millis(100));
        wait_for_group_exit(pid);
        assert!(!group_alive(pid));
    }

    #[test]
    fn terminate_kills_a_grandchild_that_outlives_the_leader() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild");
        let mut child = spawn(&format!("sleep 300 & echo $! > '{}'; wait", pidfile.display()));
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&pidfile).map_or(true, |text| text.trim().is_empty()) {
            assert!(
                Instant::now() < deadline,
                "the stand-in never wrote its grandchild's pid"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let grandchild: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        // SAFETY: signal 0 only probes whether the process exists.
        assert_eq!(unsafe { libc::kill(grandchild, 0) }, 0, "the grandchild is not running");
        terminate(&mut child);
        wait_for_group_exit(pid);
        assert!(!group_alive(pid), "a member of the group survived");
    }

    #[test]
    fn a_child_does_not_inherit_a_blocked_sigchld() {
        const CHILD: &str = "BASEMIND_AGENTS_SIGNAL_MASK_CHILD";
        // SAFETY: the calls read and set this thread's signal mask with valid pointers, and
        // the mask is restored before the test returns.
        unsafe {
            let mut mask = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask);
            if std::env::var_os(CHILD).is_some() {
                assert_eq!(libc::sigismember(&mask, libc::SIGCHLD), 0);
                return;
            }
            let mut blocked = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGCHLD);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()),
                0
            );
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "process::tests::a_child_does_not_inherit_a_blocked_sigchld",
                    "--nocapture",
                ])
                .env(CHILD, "1");
            isolate(&mut command);
            let result = command.output();
            libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
            let output = result.unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        }
    }
}

#[cfg(all(test, unix))]
mod reaped_elsewhere {
    use super::*;

    #[test]
    fn a_leader_reaped_by_someone_else_is_never_signalled() {
        for retiring in [false, true] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exit 0"]);
            isolate(&mut command);
            let mut child = command.spawn().unwrap();
            let pid = child.id();
            // A foreign reaper, such as a `waitpid(-1)` loop elsewhere in the launcher.
            let mut status = 0;
            // SAFETY: waitpid writes the status into a valid integer.
            assert_eq!(
                unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) },
                pid as libc::pid_t
            );
            if retiring {
                retire(&mut child, EXIT_GRACE + TERM_GRACE, &AtomicBool::new(false), true);
            } else {
                terminate(&mut child);
            }
            kill_group(pid);
            let signalled = SIGNALLED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(!signalled.contains(&pid), "the reused id {pid} was signalled");
        }
    }
}
