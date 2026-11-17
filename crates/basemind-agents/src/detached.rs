//! Processes a harness's descendants moved out of its process group, which a kill of the
//! session's group does not reach.
//!
//! `opencode acp` 2.3.0 runs `opencode serve --stdio`, its MCP servers and plugins in a group of
//! their own; they exit once the server's input closes, about 1.7 s after the session's group
//! died, and once took more than 3 s. While a session of such a flavor runs, the process table
//! is read every [`POLL`] and every process taken for the session's by these rules is recorded
//! by pid and start time: a descendant of the harness by parent links (to a parent
//! that started no later than the child), or a member of the POSIX session the harness or a
//! recorded descendant leads, while that leader is in the same listing.
//!
//! Once the session has ended, [`Detached::reap`] waits for those processes to exit and kills
//! what is left: the processes recorded, and members of a recorded session whose leader is
//! still listed. A process group is signalled only when every process in it is the session's;
//! otherwise each of the session's processes in it is signalled alone, and each only after its
//! pid and start time were checked again (through a pidfd on Linux 5.3 and later, which closes
//! the gap between that check and the signal; before 5.3, and on macOS, the gap remains).
//! Nothing is killed without a listing of the process table. A descendant that left before
//! the first listing, or started and left between two, is not seen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::procinfo::{self, ProcId, ProcInfo};

/// How often a running session's descendants are listed.
pub(crate) const POLL: Duration = Duration::from_millis(200);
/// How long an ended session's detached processes get to exit on their own before they are
/// killed.
pub(crate) const EXIT_GRACE: Duration = Duration::from_secs(2);
/// The most processes recorded for one session.
const MAX_PROCS: usize = 1024;
/// The most sessions a session's descendants may lead and be followed into.
const MAX_SESSIONS: usize = 64;

/// What was recorded for one session.
#[derive(Debug, Default)]
struct Record {
    /// The harness, while it runs.
    leader: Option<ProcId>,
    /// The session's processes other than the leader.
    procs: BTreeSet<ProcId>,
    /// Session ids led by a recorded process, the harness's own included, each with the start
    /// time of its leader: a process in one of them started at or after it.
    sessions: BTreeSet<ProcId>,
    /// A process or session of the session's could not be recorded for want of room.
    overflowed: bool,
}

/// The processes recorded for one session.
#[derive(Debug, Default)]
pub(crate) struct Detached {
    record: Mutex<Record>,
}

impl Detached {
    fn record(&self) -> std::sync::MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts recording the session whose harness is `leader`.
    pub fn follow(&self, leader: ProcId) {
        let mut record = self.record();
        record.leader = Some(leader);
        record.sessions.insert(leader);
    }

    /// Whether a process or session of the session's went unrecorded because the record was
    /// full: what is not recorded is not reaped, so the session must not run on.
    pub fn overflowed(&self) -> bool {
        self.record().overflowed
    }

    /// Whether `id` was recorded as one of the session's processes.
    pub fn contains(&self, id: ProcId) -> bool {
        self.record().procs.contains(&id)
    }

    /// Reads the process table and records what [`Self::observe`] finds. Returns `false` once
    /// the leader is gone, after which polling stops: its pid may name another process next.
    pub fn poll(&self) -> bool {
        match procinfo::all() {
            Some(procs) => self.observe(&procs),
            // No listing this time: try again, nothing is concluded from it.
            None => true,
        }
    }

    /// Records the session's processes in `procs`; `false` when the leader is not among them.
    pub fn observe(&self, procs: &[ProcInfo]) -> bool {
        let mut record = self.record();
        let Some(leader) = record.leader else {
            return false;
        };
        if !procs.iter().any(|proc| proc.id() == leader && !proc.zombie) {
            record.leader = None;
            return false;
        }
        let listed = listed(procs);
        // The session's processes by pid, with their start times.
        let mut ours: BTreeMap<i32, u64> = BTreeMap::from([(leader.pid, leader.start)]);
        // Parent links first, to a fixed point: a child is listed with a pid larger or smaller
        // than its parent's, so one pass in pid order is not enough. A parent that started
        // after its child is a later holder of the pid, not its parent.
        loop {
            let before = ours.len();
            for proc in procs {
                let parent_ours = ours.get(&proc.ppid).is_some_and(|start| *start <= proc.start);
                if (parent_ours || in_sessions(&record.sessions, &listed, proc)) && !ours.contains_key(&proc.pid) {
                    ours.insert(proc.pid, proc.start);
                    remember(&mut record, proc);
                }
            }
            if ours.len() == before {
                return true;
            }
        }
    }

    /// Waits up to `grace` for every recorded process to exit, then kills what is left.
    /// Returns how many signals were sent.
    pub fn reap(&self, grace: Duration) -> usize {
        self.reap_with(grace, procinfo::all)
    }

    /// As [`Self::reap`], reading the process table with `list`. Nothing is killed without a
    /// listing: when none could be made by the end of `grace`, nothing is.
    fn reap_with(&self, grace: Duration, mut list: impl FnMut() -> Option<Vec<ProcInfo>>) -> usize {
        let deadline = Instant::now().checked_add(grace);
        loop {
            let past = deadline.is_none_or(|deadline| Instant::now() >= deadline);
            if let Some(procs) = list() {
                let left = self.left(&procs);
                if left.is_empty() {
                    return 0;
                }
                if past {
                    return kill(&procs, &left);
                }
            } else if past {
                return 0;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The session's processes still running in `procs`.
    fn left(&self, procs: &[ProcInfo]) -> Vec<ProcInfo> {
        let record = self.record();
        let listed = listed(procs);
        procs
            .iter()
            .filter(|proc| {
                !proc.zombie && (record.procs.contains(&proc.id()) || in_sessions(&record.sessions, &listed, proc))
            })
            .copied()
            .collect()
    }
}

/// Every process in `procs`, zombies included.
fn listed(procs: &[ProcInfo]) -> BTreeSet<ProcId> {
    procs.iter().map(ProcInfo::id).collect()
}

/// Whether `proc` is in a session a recorded process leads, while that leader is in the same
/// listing (alive or a zombie): once it is gone its pid, and so the session id, can name an
/// unrelated session, and only the processes recorded individually are the session's.
fn in_sessions(sessions: &BTreeSet<ProcId>, listed: &BTreeSet<ProcId>, proc: &ProcInfo) -> bool {
    sessions
        .iter()
        .any(|leader| listed.contains(leader) && proc.sid == leader.pid && proc.start >= leader.start)
}

fn remember(record: &mut Record, proc: &ProcInfo) {
    if record.procs.len() < MAX_PROCS {
        record.procs.insert(proc.id());
    } else if !record.procs.contains(&proc.id()) {
        record.overflowed = true;
    }
    if proc.sid == proc.pid {
        if record.sessions.len() < MAX_SESSIONS {
            record.sessions.insert(proc.id());
        } else if !record.sessions.contains(&proc.id()) {
            record.overflowed = true;
        }
    }
}

/// What to signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// A whole process group, every member of which is the session's.
    Group(i32),
    /// One process.
    Process(ProcId),
}

/// How to kill `left`, the session's processes in `procs`: a whole group when every member of
/// it is in `left` and it is neither init's nor this process's, else each process alone.
fn plan(procs: &[ProcInfo], left: &[ProcInfo], own_group: Option<i32>) -> Vec<Target> {
    let groups: BTreeSet<i32> = left.iter().map(|proc| proc.pgid).collect();
    let mut targets = Vec::new();
    for group in groups {
        let whole = procs
            .iter()
            .filter(|proc| proc.pgid == group && !proc.zombie)
            .all(|member| left.iter().any(|ours| ours.id() == member.id()));
        if whole && group > 1 && Some(group) != own_group {
            targets.push(Target::Group(group));
        } else {
            targets.extend(
                left.iter()
                    .filter(|proc| proc.pgid == group)
                    .map(|proc| Target::Process(proc.id())),
            );
        }
    }
    targets
}

/// Kills `left`, the session's processes in `procs`, as [`plan`] says. Returns how many
/// signals were sent.
fn kill(procs: &[ProcInfo], left: &[ProcInfo]) -> usize {
    let mut sent = 0;
    for target in plan(procs, left, own_group()) {
        let signalled = match target {
            Target::Group(group) => {
                let members: Vec<&ProcInfo> = left.iter().filter(|proc| proc.pgid == group).collect();
                signal_group(group, &members)
            }
            Target::Process(id) => signal_process(id),
        };
        sent += usize::from(signalled);
    }
    sent
}

fn own_group() -> Option<i32> {
    #[cfg(unix)]
    {
        // SAFETY: getpgrp has no preconditions and cannot fail.
        Some(unsafe { libc::getpgrp() })
    }
    #[cfg(not(unix))]
    None
}

/// Kills group `group` after checking again that each of `members` is still the process it
/// was and still in the group, and that the group has no member besides them.
fn signal_group(group: i32, members: &[&ProcInfo]) -> bool {
    let unchanged = members
        .iter()
        .all(|member| procinfo::info(member.pid).is_some_and(|now| now.start == member.start && now.pgid == group));
    if !unchanged || !group_is_only(group, members) {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: a negative pid signals that process group; it is not init's, the kernel's or
        // this process's, and every member was just checked to be the session's.
        unsafe { libc::kill(-group, libc::SIGKILL) == 0 }
    }
    #[cfg(not(unix))]
    false
}

/// Whether a fresh listing shows no process in `group` besides `members`.
fn group_is_only(group: i32, members: &[&ProcInfo]) -> bool {
    procinfo::all().is_some_and(|procs| {
        procs
            .iter()
            .filter(|proc| proc.pgid == group && !proc.zombie)
            .all(|proc| members.iter().any(|member| member.id() == proc.id()))
    })
}

/// Kills the one process `id`, if it is still that process.
fn signal_process(id: ProcId) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // A pidfd pins the process: once it is open and the process checked, the signal cannot
        // reach another process that took the pid.
        // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, id.pid, 0) };
        if let Ok(fd) = libc::c_int::try_from(fd)
            && fd >= 0
        {
            // SAFETY: `fd` was just returned by pidfd_open and is owned here.
            let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            if !procinfo::running(id) {
                return false;
            }
            let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
            // SAFETY: pidfd_send_signal takes the pidfd, a signal and a null siginfo.
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    raw,
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            return sent == 0;
        }
    }
    #[cfg(unix)]
    {
        if !procinfo::running(id) {
            return false;
        }
        // SAFETY: kill takes plain integers; the pid was just checked to be the process
        // recorded, and only a reuse in the instant since could misdirect it.
        unsafe { libc::kill(id.pid, libc::SIGKILL) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = id;
        false
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn proc(pid: i32, ppid: i32, pgid: i32, sid: i32) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            pgid,
            sid,
            start: 100,
            zombie: false,
        }
    }

    fn leader() -> ProcId {
        ProcId { pid: 500, start: 100 }
    }

    #[test]
    fn descendants_and_session_members_are_recorded_and_nothing_else() {
        let procs = [
            proc(500, 1, 500, 500),
            // In the leader's group.
            proc(501, 500, 500, 500),
            // A detached server in a group of its own, and its child.
            proc(502, 500, 502, 500),
            proc(503, 502, 502, 500),
            // A descendant that left for a session of its own, and its child listed first.
            proc(499, 504, 504, 504),
            proc(504, 501, 504, 504),
            // Unrelated, in a group and session of its own.
            proc(900, 1, 900, 900),
        ];
        let detached = Detached::default();
        detached.follow(leader());
        assert!(detached.observe(&procs));
        let recorded: Vec<i32> = detached.record().procs.iter().map(|id| id.pid).collect();
        assert_eq!(recorded, [499, 501, 502, 503, 504]);
        assert!(!detached.contains(procs[6].id()));
        // A reused pid is not the recorded process.
        assert!(!detached.contains(ProcId { pid: 502, start: 101 }));
    }

    #[test]
    fn a_session_id_counts_only_while_its_leader_is_listed() {
        let detached = Detached::default();
        detached.follow(leader());
        // A descendant leads a session of its own; its member is recorded.
        detached.observe(&[
            proc(500, 1, 500, 500),
            proc(504, 500, 504, 504),
            proc(505, 504, 504, 504),
        ]);
        // 504 is gone and an unrelated process took its pid and leads a session of that id, as
        // a new terminal would; it and its member are not the session's, alive leader or not.
        let reused = ProcInfo {
            start: 300,
            ..proc(504, 1, 504, 504)
        };
        let member = ProcInfo {
            start: 301,
            ..proc(606, 504, 504, 504)
        };
        assert!(detached.observe(&[proc(500, 1, 500, 500), reused, member]));
        assert!(!detached.contains(reused.id()) && !detached.contains(member.id()));
        assert!(detached.left(&[reused, member]).is_empty());
        // The members recorded while their leader lived are still the session's.
        assert_eq!(detached.left(&[proc(505, 1, 504, 504)]), [proc(505, 1, 504, 504)]);
    }

    #[test]
    fn a_parent_link_counts_only_to_a_parent_older_than_its_child() {
        let detached = Detached::default();
        detached.follow(leader());
        // 700 names 501 as its parent but started before it: 501 is a later holder of the pid.
        let older = ProcInfo {
            start: 50,
            ..proc(700, 501, 700, 700)
        };
        let child = ProcInfo {
            start: 150,
            ..proc(501, 500, 500, 500)
        };
        detached.observe(&[proc(500, 1, 500, 500), child, older]);
        assert!(detached.contains(child.id()));
        assert!(
            !detached.contains(older.id()),
            "a process was taken for the child of a newer one"
        );
    }

    #[test]
    fn a_session_with_more_processes_than_can_be_recorded_overflows() {
        let detached = Detached::default();
        detached.follow(leader());
        let mut procs = vec![proc(500, 1, 500, 500)];
        procs.extend((0..MAX_PROCS + 10).map(|at| proc(1000 + i32::try_from(at).unwrap(), 500, 500, 500)));
        assert!(detached.observe(&procs));
        assert!(detached.overflowed(), "processes went unrecorded without a sign");
        let fits = Detached::default();
        fits.follow(leader());
        fits.observe(&procs[..10]);
        assert!(!fits.overflowed());
    }

    #[test]
    fn polling_stops_once_the_leader_is_gone_or_its_pid_reused() {
        let detached = Detached::default();
        detached.follow(leader());
        let reused = [ProcInfo {
            start: 999,
            ..proc(500, 1, 500, 500)
        }];
        assert!(!detached.observe(&reused));
        // Nothing was recorded from the process that took the pid, and nothing more will be.
        assert!(!detached.observe(&[proc(500, 1, 500, 500), proc(501, 500, 500, 500)]));
        assert!(detached.record().procs.is_empty());
    }

    #[test]
    fn a_group_is_killed_whole_only_when_every_member_is_the_sessions() {
        let ours = [proc(502, 1, 502, 500), proc(503, 502, 502, 500), proc(510, 1, 510, 500)];
        // 510's group also holds a process that is not the session's (a reused pid would do).
        let stranger = ProcInfo {
            start: 999,
            ..proc(511, 1, 510, 500)
        };
        let mut procs = ours.to_vec();
        procs.push(stranger);
        assert_eq!(
            plan(&procs, &ours, None),
            [Target::Group(502), Target::Process(ours[2].id())]
        );
        // This process's own group, and init's, are never signalled whole.
        assert_eq!(
            plan(&ours[..2], &ours[..2], Some(502)),
            [Target::Process(ours[0].id()), Target::Process(ours[1].id())]
        );
        let in_init = [proc(520, 1, 1, 500)];
        assert_eq!(plan(&in_init, &in_init, None), [Target::Process(in_init[0].id())]);
    }

    #[test]
    fn nothing_is_killed_without_a_listing() {
        let detached = Detached::default();
        detached.follow(leader());
        detached.observe(&[proc(500, 1, 500, 500), proc(502, 500, 502, 500)]);
        let mut listings = 0;
        let sent = detached.reap_with(Duration::from_millis(120), || {
            listings += 1;
            None
        });
        assert_eq!(sent, 0);
        assert!(listings > 1, "the reaper gave up before its grace was over");
    }

    #[test]
    fn only_the_sessions_processes_are_left_to_kill() {
        let detached = Detached::default();
        detached.follow(leader());
        detached.observe(&[proc(500, 1, 500, 500), proc(502, 500, 502, 500)]);
        // The leader is gone; a process that reused the server's pid is not the server.
        let later = [ProcInfo {
            start: 200,
            ..proc(502, 1, 502, 777)
        }];
        assert!(detached.left(&later).is_empty());
        let still = [proc(502, 1, 502, 500)];
        assert_eq!(detached.left(&still), still);
    }
}
