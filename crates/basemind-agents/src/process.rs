// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/process.rs) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! Process-group handling: a harness is started as the leader of its own group, so ending the
//! session ends every helper it spawned and leaves no orphan behind.

use std::process::{Child, Command};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How long a harness whose stdin was closed may take to exit by itself.
#[cfg(unix)]
pub(crate) const EXIT_GRACE: Duration = Duration::from_millis(1500);
/// How long the group leader may take to shut down after SIGTERM.
#[cfg(unix)]
pub(crate) const TERM_GRACE: Duration = Duration::from_secs(2);

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
                let mut signals = std::mem::zeroed();
                libc::sigemptyset(&mut signals);
                if libc::sigprocmask(libc::SIG_SETMASK, &signals, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error());
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

/// Kills every process of the group `pid` leads. The caller must know the leader has not
/// been reaped, or the id may belong to another process by now.
pub(crate) fn kill_group(pid: u32) {
    #[cfg(unix)]
    // SAFETY: kill takes plain integers; a negative pid addresses the process group.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut kill = Command::new("taskkill");
        kill.args(["/PID", &pid.to_string(), "/T", "/F"]);
        kill.creation_flags(CREATE_NO_WINDOW);
        let _ = kill.status();
    }
}

/// Kills the group at once and reaps the leader.
pub(crate) fn terminate(child: &mut Child) {
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Ends a harness whose work is done without cutting off a write it has in flight. Close its
/// stdin before calling this.
///
/// Claude Code rotates its OAuth refresh token on every refresh and saves the new one through
/// a helper process. Killing the group in that window loses the token and signs the account
/// out. So: wait for an exit on stdin EOF, then SIGTERM the leader only, and SIGKILL the group
/// last. The group is always killed before the leader is reaped, while its id is still ours.
pub(crate) fn retire(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        if !exited(pid, EXIT_GRACE) {
            // SAFETY: kill takes plain integers; the leader is unreaped, so the id is ours.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            exited(pid, TERM_GRACE);
        }
    }
    terminate(child);
}

/// Whether the child `pid` exits within `within`, observed without reaping it.
#[cfg(unix)]
fn exited(pid: u32, within: Duration) -> bool {
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
        if status != 0 || reported != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
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
        retire(&mut child);
        assert!(started.elapsed() < EXIT_GRACE);
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "saved");
    }

    #[test]
    fn retire_escalates_when_a_child_ignores_stdin_and_sigterm() {
        let mut child = spawn("trap '' TERM; while :; do sleep 0.05; done");
        let pid = child.id();
        drop(child.stdin.take());
        let started = Instant::now();
        retire(&mut child);
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
