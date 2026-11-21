//! Requests a long-lived daemon serves over and over must not fan out onto threads they create
//! per call. Each such thread is a stack mapped and unmapped and an allocator heap taken and
//! abandoned; tens of thousands of them over a soak fragmented the daemon's heap.
//!
//! The guard samples the process's live thread count while the request runs, so it lives alone in
//! this test binary: a sibling test spawning threads would read as the request's.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Threads one working-tree status may run besides the caller's: gix's two producers (tree to
/// index, index to worktree) and, inside the latter, its directory walk and its comparison.
const STATUS_THREADS: usize = 4;

fn live_threads() -> usize {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: `info` is a writable `proc_taskinfo` of exactly `size` bytes, the buffer
        // PROC_PIDTASKINFO fills for this process's pid.
        let written = unsafe {
            libc::proc_pidinfo(
                std::process::id() as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        assert_eq!(written, size, "proc_pidinfo(PROC_PIDTASKINFO)");
        usize::try_from(info.pti_threadnum).expect("thread count")
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
        status
            .lines()
            .find_map(|line| line.strip_prefix("Threads:"))
            .and_then(|count| count.trim().parse().ok())
            .expect("Threads: line")
    }
}

fn git(root: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(root)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// Wait until no more than `baseline` threads are alive. On Linux a thread that `join` returned for
/// still counts in `/proc/self/status` until the kernel has released it, a moment after its exit
/// woke the joiner; a call started inside that moment would count its predecessor's threads as its
/// own. A thread that never goes is a leak, not a straggler.
fn settle_to(baseline: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while live_threads() > baseline {
        assert!(
            std::time::Instant::now() < deadline,
            "{} threads still alive 5 s after a status, {baseline} before it: a status leaves threads behind",
            live_threads()
        );
        std::thread::yield_now();
    }
}

/// The most threads alive at once, beyond those alive before, while any one of `calls` runs. Each
/// call starts once the previous one's threads are gone, so only the threads a call creates count.
fn peak_extra_threads(calls: usize, mut work: impl FnMut()) -> usize {
    let done = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let (done, peak) = (Arc::clone(&done), Arc::clone(&peak));
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                peak.fetch_max(live_threads(), Ordering::Relaxed);
            }
        })
    };
    // Wait for the sampler to see itself, so it is part of the baseline.
    while peak.load(Ordering::Relaxed) == 0 {
        std::hint::spin_loop();
    }
    let baseline = live_threads();
    for _ in 0..calls {
        settle_to(baseline);
        work();
    }
    done.store(true, Ordering::Relaxed);
    sampler.join().expect("sampler");
    peak.load(Ordering::Relaxed).saturating_sub(baseline)
}

/// `git_diff {}` answers from a working-tree status. gix fanned its comparison out to a thread per
/// core on every call, so a 14-core machine created about eighteen threads per status.
#[test]
fn a_working_tree_status_runs_on_a_bounded_set_of_threads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    for i in 0..600 {
        std::fs::write(root.join(format!("f{i:03}.rs")), format!("fn f{i}() {{}}\n")).expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    for i in (0..600).step_by(7) {
        std::fs::write(root.join(format!("f{i:03}.rs")), format!("fn g{i}() {{}}\n")).expect("rewrite");
    }
    std::fs::write(root.join("untracked.rs"), "fn u() {}\n").expect("write");
    let repo = basemind::git::Repo::discover(root).expect("discover");

    let mut modified = 0;
    let peak = peak_extra_threads(20, || {
        modified = repo.status_porcelain().expect("status").modified.len();
    });
    assert_eq!(modified, 600_usize.div_ceil(7), "the status is still complete");
    assert!(
        peak <= STATUS_THREADS,
        "a status ran on {peak} threads besides the caller; at most {STATUS_THREADS} are expected"
    );
}
