//! Process groups a harness's descendants moved into, which a kill of the session's group does
//! not reach.
//!
//! `opencode acp` 2.3.0 runs `opencode serve --stdio`, its MCP servers and plugins in a group of
//! their own; they exit once the server's input closes, about 1.7 s after the session's group
//! died, and once took more than 3 s. While a session of such a flavor runs, its descendants are
//! listed with `ps` and every group they lead other than the session's own is recorded. Once
//! the session has ended, [`Detached::reap`] waits for those groups to empty and kills what is
//! left. A group is only signalled while a process recorded in it is still a member, so a group
//! id reused by an unrelated process is left alone; between that check and the signal the id
//! could still be reused, which is not prevented. A descendant that left before the first
//! listing, or that started and left between two listings, is not seen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often a running session's descendants are listed.
pub(crate) const POLL: Duration = Duration::from_millis(200);
/// How long an ended session's detached groups get to exit on their own before they are killed.
pub(crate) const EXIT_GRACE: Duration = Duration::from_secs(2);
/// The most groups recorded for one session.
const MAX_GROUPS: usize = 64;
/// The most processes recorded in one group.
const MAX_MEMBERS: usize = 256;

/// One process as `ps` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
}

/// The groups recorded for one session, each with the processes seen in it.
#[derive(Debug, Default)]
pub(crate) struct Detached {
    groups: Mutex<BTreeMap<i32, BTreeSet<i32>>>,
}

impl Detached {
    fn groups(&self) -> std::sync::MutexGuard<'_, BTreeMap<i32, BTreeSet<i32>>> {
        self.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The recorded group ids.
    pub fn group_ids(&self) -> Vec<i32> {
        self.groups().keys().copied().collect()
    }

    /// Records the groups other than the leader's that the leader's descendants in `procs` are in.
    pub fn observe(&self, leader: u32, procs: &[Proc]) {
        let Ok(leader) = i32::try_from(leader) else {
            return;
        };
        let mut children: BTreeMap<i32, Vec<&Proc>> = BTreeMap::new();
        for proc in procs {
            children.entry(proc.ppid).or_default().push(proc);
        }
        let mut groups = self.groups();
        let mut queue = vec![leader];
        let mut seen = BTreeSet::new();
        while let Some(parent) = queue.pop() {
            if !seen.insert(parent) {
                continue;
            }
            for child in children.get(&parent).into_iter().flatten() {
                queue.push(child.pid);
                if child.pgid == leader || !killable(child.pgid) {
                    continue;
                }
                if groups.len() >= MAX_GROUPS && !groups.contains_key(&child.pgid) {
                    continue;
                }
                let members = groups.entry(child.pgid).or_default();
                if members.len() < MAX_MEMBERS {
                    members.insert(child.pid);
                }
            }
        }
    }

    /// Lists the processes now and records what [`Self::observe`] finds among them.
    pub fn poll(&self, leader: u32) {
        if let Some(procs) = list() {
            self.observe(leader, &procs);
        }
    }

    /// Waits up to `grace` for every recorded group to empty, then kills each group that still
    /// holds a recorded process. Returns how many groups were killed.
    pub fn reap(&self, grace: Duration) -> usize {
        let deadline = Instant::now() + grace;
        loop {
            let live = self.live(list().as_deref());
            if live.is_empty() {
                return 0;
            }
            if Instant::now() >= deadline {
                for pgid in &live {
                    kill(*pgid);
                }
                return live.len();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The recorded groups that still hold a recorded process. Without a listing, every group
    /// still signalable counts.
    fn live(&self, procs: Option<&[Proc]>) -> Vec<i32> {
        let groups = self.groups();
        groups
            .iter()
            .filter(|(pgid, members)| match procs {
                Some(procs) => procs
                    .iter()
                    .any(|proc| proc.pgid == **pgid && members.contains(&proc.pid)),
                None => exists(**pgid),
            })
            .map(|(pgid, _)| *pgid)
            .collect()
    }
}

/// Whether `pgid` is a group this process may ever signal: not init's, not the kernel's, not
/// the launcher's own.
fn killable(pgid: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: getpgrp has no preconditions and cannot fail.
        pgid > 1 && pgid != unsafe { libc::getpgrp() }
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

fn exists(pgid: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the group exists and may be signalled.
        killable(pgid) && unsafe { libc::kill(-pgid, 0) } == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

fn kill(pgid: i32) {
    #[cfg(unix)]
    if killable(pgid) {
        // SAFETY: a negative pid signals that process group; `killable` excluded init's, the
        // kernel's and our own.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// The process group `pid` belongs to, while it exists.
pub(crate) fn group_of(pid: u32) -> Option<u32> {
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
        // SAFETY: getpgid only reads the process table.
        let pgid = unsafe { libc::getpgid(pid) };
        u32::try_from(pgid).ok().filter(|pgid| *pgid > 0)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Every process, from `ps`. `None` when no `ps` ran.
fn list() -> Option<Vec<Proc>> {
    let ps = ["/bin/ps", "/usr/bin/ps"]
        .into_iter()
        .find(|path| std::path::Path::new(path).is_file())?;
    let output = std::process::Command::new(ps)
        .args(["-A", "-o", "pid=,ppid=,pgid="])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&output.stdout)))
}

/// The lines of `ps -o pid=,ppid=,pgid=` that parse.
pub(crate) fn parse(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<i32>);
            let proc = Proc {
                pid: fields.next()?.ok()?,
                ppid: fields.next()?.ok()?,
                pgid: fields.next()?.ok()?,
            };
            fields.next().is_none().then_some(proc)
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn ps_output_parses_and_malformed_lines_are_skipped() {
        let procs = parse("  10   1  10\n 11 10 10\nbad line\n 12 11 12 extra\n");
        assert_eq!(
            procs,
            [
                Proc {
                    pid: 10,
                    ppid: 1,
                    pgid: 10
                },
                Proc {
                    pid: 11,
                    ppid: 10,
                    pgid: 10
                },
            ]
        );
    }

    #[test]
    fn only_descendants_in_another_group_are_recorded() {
        let procs = [
            Proc {
                pid: 500,
                ppid: 1,
                pgid: 500,
            },
            // In the leader's group: the session's own kill reaches it.
            Proc {
                pid: 501,
                ppid: 500,
                pgid: 500,
            },
            // A detached server and its child.
            Proc {
                pid: 502,
                ppid: 500,
                pgid: 502,
            },
            Proc {
                pid: 503,
                ppid: 502,
                pgid: 502,
            },
            // Unrelated, in a group of its own.
            Proc {
                pid: 900,
                ppid: 1,
                pgid: 900,
            },
            // A descendant in init's group is never recorded.
            Proc {
                pid: 504,
                ppid: 501,
                pgid: 1,
            },
        ];
        let detached = Detached::default();
        detached.observe(500, &procs);
        assert_eq!(detached.group_ids(), [502]);
        assert_eq!(detached.live(Some(&procs)), [502]);
        // The group id now belongs to processes never recorded: it is left alone.
        let reused = [Proc {
            pid: 777,
            ppid: 1,
            pgid: 502,
        }];
        assert!(detached.live(Some(&reused)).is_empty());
    }

    #[test]
    fn the_group_of_this_process_is_known() {
        // SAFETY: getpgrp has no preconditions.
        let own = unsafe { libc::getpgrp() };
        assert_eq!(group_of(std::process::id()), u32::try_from(own).ok());
        assert!(!killable(own));
        assert!(!killable(1));
        assert_eq!(group_of(0), None);
    }
}
