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
    #[cfg(test)]
    pub fn group_ids(&self) -> Vec<i32> {
        self.groups().keys().copied().collect()
    }

    /// Records the groups other than the leader's that the leader's descendants in `procs` are
    /// in. Returns `false`, recording nothing, once the leader is not listed: it was reaped, and
    /// its pid may already name another process, whose children are not the session's.
    pub fn observe(&self, leader: u32, procs: &[Proc]) -> bool {
        let Ok(leader) = i32::try_from(leader) else {
            return false;
        };
        if !procs.iter().any(|proc| proc.pid == leader) {
            return false;
        }
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
        true
    }

    /// Lists the processes now and records what [`Self::observe`] finds among them. Returns
    /// `false` once the leader is gone, after which the session's tracker stops; a failed
    /// listing concludes nothing.
    pub fn poll(&self, leader: u32) -> bool {
        list().is_none_or(|procs| self.observe(leader, &procs))
    }

    /// Waits up to `grace` for every recorded group to empty, then kills each group that still
    /// holds a recorded process. Returns how many groups were killed. Nothing is killed without
    /// a listing: when none could be made by the end of `grace`, nothing is.
    pub fn reap(&self, grace: Duration) -> usize {
        let deadline = Instant::now().checked_add(grace);
        loop {
            let past = deadline.is_none_or(|deadline| Instant::now() >= deadline);
            let Some(procs) = list() else {
                if past {
                    return 0;
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            let live = self.live(Some(&procs));
            if live.is_empty() {
                return 0;
            }
            if past {
                for pgid in &live {
                    kill(*pgid);
                }
                return live.len();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The recorded groups that still hold a recorded process. Without a listing, none.
    fn live(&self, procs: Option<&[Proc]>) -> Vec<i32> {
        let groups = self.groups();
        groups
            .iter()
            .filter(|(pgid, members)| match procs {
                Some(procs) => procs
                    .iter()
                    .any(|proc| proc.pgid == **pgid && members.contains(&proc.pid)),
                None => false,
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

/// Every process, from `ps`. `None` when no `ps` ran, or it listed nothing: this process at
/// least is always running, so an empty listing is a failed one.
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
    let procs = output
        .status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&output.stdout)))?;
    (!procs.is_empty()).then_some(procs)
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
    use std::os::unix::process::CommandExt;

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
    fn this_process_and_init_are_never_killable() {
        // SAFETY: getpgrp has no preconditions.
        let own = unsafe { libc::getpgrp() };
        assert!(!killable(own));
        assert!(!killable(1));
    }

    #[test]
    fn a_leader_missing_from_the_listing_stops_the_tracker_and_records_nothing() {
        // The leader 500 was reaped; whatever lists 500 as its parent now is another process's
        // child, the pid having been reused.
        let procs = [Proc {
            pid: 600,
            ppid: 500,
            pgid: 600,
        }];
        let detached = Detached::default();
        let tracking = detached.observe(500, &procs);
        assert!(detached.group_ids().is_empty(), "a reused pid's child was recorded");
        assert!(!tracking, "the tracker kept polling a dead leader");
    }

    #[test]
    fn nothing_is_reaped_without_a_listing() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let detached = Detached::default();
        detached.observe(
            1,
            &[Proc {
                pid,
                ppid: 1,
                pgid: pid,
            }],
        );
        let live = detached.live(None);
        let _ = child.kill();
        let _ = child.wait();
        assert!(live.is_empty(), "a group was counted live without a listing: {live:?}");
    }
}
