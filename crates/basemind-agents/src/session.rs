// The spawn retry is adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/provider/parked.rs) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! The session handle and the supervisor behind it: one thread runs the adapter, one enforces
//! the limits, and the process group is gone before `Finished` is reported.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::error::LaunchError;
use crate::event::{ApprovalRequest, ApprovalSource, Control, Event, FinishStatus};
use crate::process;
use crate::spec::{ApprovalPolicy, ApprovalVerdict, LaunchSpec, Limits};

const STDERR_TAIL_BYTES: usize = 8192;
const STDERR_DRAIN: Duration = Duration::from_millis(200);

/// The exact process an adapter would start: its program, arguments, environment and working
/// directory. Built without side effects, so a caller can log or inspect a launch beforehand.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommandPlan {
    /// The executable.
    pub program: PathBuf,
    /// Every argument, in order.
    pub args: Vec<String>,
    /// The complete environment; the child inherits nothing else.
    pub env: BTreeMap<String, OsString>,
    /// The working directory, canonical and inside the spec's root.
    pub cwd: PathBuf,
}

/// The pipes of a started harness.
pub(crate) struct ChildIo {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

/// The protocol side of one session. `run` returns when the session's turns are over.
pub(crate) trait Driver: Send + 'static {
    fn run(self: Box<Self>, io: ChildIo, sink: EventSink) -> Result<(), LaunchError>;
}

/// Hands a control message to the driver; `false` when the driver is gone.
pub(crate) type ControlTx = Box<dyn Fn(Control) -> bool + Send + Sync>;

struct State {
    finished: bool,
    forced: Option<FinishStatus>,
    stop_deadline: Option<Instant>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    pid: u32,
    limits: Limits,
    policy: Option<Arc<dyn ApprovalPolicy>>,
    control: ControlTx,
    /// Turns the adapter has begun.
    turns: AtomicU32,
    /// Prompts the caller has handed over, the brief included.
    prompts: AtomicU32,
}

impl Shared {
    fn new(pid: u32, limits: Limits, policy: Option<Arc<dyn ApprovalPolicy>>, control: ControlTx) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                finished: false,
                forced: None,
                stop_deadline: None,
            }),
            wake: Condvar::new(),
            pid,
            limits,
            policy,
            control,
            turns: AtomicU32::new(0),
            prompts: AtomicU32::new(1),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Where an adapter reports to. It also holds the two decisions an adapter must not make by
/// itself: whether a turn may begin, and what an approval request is answered with.
#[derive(Clone)]
pub(crate) struct EventSink {
    tx: Sender<Event>,
    shared: Arc<Shared>,
}

impl EventSink {
    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Counts a turn the harness is about to run, and refuses one beyond the limit.
    pub fn begin_turn(&self) -> Result<u32, LaunchError> {
        let max = self.shared.limits.max_turns;
        let turn = self.shared.turns.load(Ordering::SeqCst) + 1;
        if turn > max {
            return Err(LaunchError::TurnLimit(max));
        }
        self.shared.turns.store(turn, Ordering::SeqCst);
        Ok(turn)
    }

    pub fn turns(&self) -> u32 {
        self.shared.turns.load(Ordering::SeqCst)
    }

    pub fn max_turns(&self) -> u32 {
        self.shared.limits.max_turns
    }

    /// Reports the request and answers it: denied without a policy, else the policy's verdict.
    /// On `Ask` the adapter keeps the request pending for the caller.
    pub fn request_approval(&self, request: ApprovalRequest) -> ApprovalVerdict {
        let id = request.id.clone();
        let (verdict, source) = match &self.shared.policy {
            None => (ApprovalVerdict::Deny, ApprovalSource::DefaultDeny),
            Some(policy) => (policy.decide(&request), ApprovalSource::Policy),
        };
        self.emit(Event::ApprovalRequest(request));
        if verdict != ApprovalVerdict::Ask {
            self.resolved(id, verdict == ApprovalVerdict::Allow, source);
        }
        verdict
    }

    pub fn resolved(&self, id: String, allowed: bool, source: ApprovalSource) {
        self.emit(Event::ApprovalResolved { id, allowed, source });
    }

    #[cfg(test)]
    pub fn for_test(policy: Option<Arc<dyn ApprovalPolicy>>, max_turns: u32) -> (Self, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let limits = Limits {
            max_turns,
            ..Limits::default()
        };
        (
            Self {
                tx,
                shared: Shared::new(0, limits, policy, Box::new(|_| false)),
            },
            rx,
        )
    }
}

/// A running agent in another harness.
///
/// Events arrive in order and end with [`Event::Finished`]. Dropping the session kills the
/// harness's process group.
pub struct Session {
    events: Receiver<Event>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("pid", &self.shared.pid)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// The process id of the harness, which is also its process group id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// The next event, blocking until there is one. `None` once [`Event::Finished`] was read.
    #[must_use]
    pub fn recv(&self) -> Option<Event> {
        self.events.recv().ok()
    }

    /// The next event, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// [`RecvTimeoutError::Timeout`] when none arrived in time, and
    /// [`RecvTimeoutError::Disconnected`] once [`Event::Finished`] was read.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    /// Every remaining event, blocking between them, ending after [`Event::Finished`].
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.iter()
    }

    /// Allows the pending approval request `id`, once.
    ///
    /// # Errors
    ///
    /// [`LaunchError::SessionClosed`] when the session has finished.
    pub fn approve(&self, id: &str) -> Result<(), LaunchError> {
        self.send(Control::Answer {
            id: id.to_owned(),
            allow: true,
        })
    }

    /// Denies the pending approval request `id`.
    ///
    /// # Errors
    ///
    /// [`LaunchError::SessionClosed`] when the session has finished.
    pub fn deny(&self, id: &str) -> Result<(), LaunchError> {
        self.send(Control::Answer {
            id: id.to_owned(),
            allow: false,
        })
    }

    /// Queues `text` as the next prompt; it starts a new turn when the current one ends.
    ///
    /// # Errors
    ///
    /// [`LaunchError::TurnLimit`] when the limits allow no further turn, and
    /// [`LaunchError::SessionClosed`] when the session has finished.
    pub fn send_follow_up(&self, text: impl Into<String>) -> Result<(), LaunchError> {
        let max = self.shared.limits.max_turns;
        self.shared
            .prompts
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |prompts| {
                (prompts < max).then_some(prompts + 1)
            })
            .map_err(|_| LaunchError::TurnLimit(max))?;
        self.send(Control::FollowUp(text.into()))
    }

    /// Asks the harness to stop. It is interrupted through its own protocol and given the
    /// stop grace of the limits to wind down; then its process group is killed.
    pub fn stop(&self) {
        self.request_stop(self.shared.limits.stop_grace);
    }

    /// Blocks until the session is over and returns how it ended.
    #[must_use]
    pub fn wait(self) -> FinishStatus {
        self.events()
            .find_map(|event| match event {
                Event::Finished(status) => Some(status),
                _ => None,
            })
            .unwrap_or(FinishStatus::Failed)
    }

    fn send(&self, control: Control) -> Result<(), LaunchError> {
        if self.shared.state().finished || !(self.shared.control)(control) {
            return Err(LaunchError::SessionClosed);
        }
        Ok(())
    }

    fn request_stop(&self, grace: Duration) {
        {
            let mut state = self.shared.state();
            if state.finished {
                return;
            }
            state.forced.get_or_insert(FinishStatus::Stopped);
            let deadline = Instant::now() + grace;
            state.stop_deadline = Some(state.stop_deadline.map_or(deadline, |at| at.min(deadline)));
        }
        self.shared.wake.notify_all();
        (self.shared.control)(Control::Stop);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.request_stop(Duration::ZERO);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Starts `plan` as the leader of its own process group and runs `driver` over its pipes.
pub(crate) fn start(
    plan: &CommandPlan,
    spec: &LaunchSpec,
    driver: Box<dyn Driver>,
    control: ControlTx,
) -> Result<Session, LaunchError> {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .env_clear()
        .envs(&plan.env)
        .current_dir(&plan.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process::isolate(&mut command);
    let mut child = spawn(&mut command).map_err(|source| LaunchError::Spawn {
        executable: plan.program.clone(),
        source,
    })?;
    let piped = "the child was spawned with piped stdio";
    let io = ChildIo {
        stdin: child.stdin.take().expect(piped),
        stdout: child.stdout.take().expect(piped),
    };
    let mut stderr = child.stderr.take().expect(piped);

    let shared = Shared::new(child.id(), spec.limits, spec.approval_policy.clone(), control);
    let (tx, events) = mpsc::channel();
    let sink = EventSink {
        tx,
        shared: shared.clone(),
    };

    let tail = Arc::new(Mutex::new(Vec::new()));
    let (stderr_done, stderr_drained) = mpsc::channel::<()>();
    std::thread::spawn({
        let tail = tail.clone();
        move || {
            let mut chunk = [0; 4096];
            while let Ok(read) = stderr.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                let mut tail = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                tail.extend_from_slice(&chunk[..read]);
                let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                tail.drain(..excess);
            }
            drop(stderr_done);
        }
    });

    std::thread::spawn({
        let shared = shared.clone();
        let started = Instant::now();
        move || watchdog(&shared, started)
    });

    let worker = std::thread::spawn({
        let shared = shared.clone();
        move || {
            let run = std::panic::AssertUnwindSafe(|| driver.run(io, sink.clone()));
            let result = std::panic::catch_unwind(run)
                .unwrap_or_else(|_| Err(LaunchError::Protocol("the adapter panicked".to_owned())));
            let status = {
                let mut state = shared.state();
                state.finished = true;
                match (state.forced, &result) {
                    (Some(forced), _) => forced,
                    (None, Ok(())) => FinishStatus::Completed,
                    (None, Err(LaunchError::TurnLimit(_))) => FinishStatus::TurnLimitExceeded,
                    (None, Err(_)) => FinishStatus::Failed,
                }
            };
            shared.wake.notify_all();
            // From here on only this thread signals the group: the watchdog saw `finished`.
            if matches!(status, FinishStatus::Completed | FinishStatus::Stopped) {
                process::retire(&mut child);
            } else {
                process::terminate(&mut child);
            }
            if let (FinishStatus::Failed, Err(error)) = (status, &result) {
                let _ = stderr_drained.recv_timeout(STDERR_DRAIN);
                let tail = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let tail = String::from_utf8_lossy(&tail);
                let tail = tail.trim();
                sink.emit(Event::Error(if tail.is_empty() {
                    error.to_string()
                } else {
                    format!("{error}\n{tail}")
                }));
            }
            sink.emit(Event::Finished(status));
        }
    });

    Ok(Session {
        events,
        shared,
        worker: Some(worker),
    })
}

/// Spawns, retrying while the executable is still open for writing: a harness that was
/// installed or updated a moment ago fails with ETXTBSY until its writer closes the file.
fn spawn(command: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    for attempt in 1..5 {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(25 * attempt));
            }
            settled => return settled,
        }
    }
    command.spawn()
}

/// Kills the process group at the wall-time deadline, or once a stop has had its grace.
fn watchdog(shared: &Shared, started: Instant) {
    let wall = started + shared.limits.wall_time;
    let mut state = shared.state();
    loop {
        if state.finished {
            return;
        }
        let now = Instant::now();
        let stop_due = state.stop_deadline.is_some_and(|at| now >= at);
        if now >= wall || stop_due {
            if !stop_due {
                state.forced.get_or_insert(FinishStatus::WallTimeExceeded);
            }
            // `finished` is false under the lock, so the leader is unreaped and the id is ours.
            process::kill_group(shared.pid);
            drop(state);
            // A descendant that left the group can hold the pipes open; the adapter is told
            // to stop instead of being left to wait for an end of output that never comes.
            (shared.control)(Control::Stop);
            return;
        }
        let until = state.stop_deadline.map_or(wall, |at| at.min(wall));
        state = shared
            .wake
            .wait_timeout(state, until - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}
