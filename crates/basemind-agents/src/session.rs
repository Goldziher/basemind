// The spawn retry is adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/provider/parked.rs) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! The session handle and the supervisor behind it: one thread runs the adapter, one enforces
//! the limits, and the process group is gone before `Finished` is reported.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::error::LaunchError;
use crate::event::{ApprovalRequest, ApprovalSource, Control, Event, FinishStatus};
use crate::pipe;
use crate::process;
use crate::queue::EventQueue;
use crate::spec::{ApprovalPolicy, ApprovalVerdict, LaunchSpec, Limits};

const STDERR_TAIL_BYTES: usize = 8192;
/// How many approval requests may wait for the caller at once; a further one is denied.
pub(crate) const MAX_PENDING_APPROVALS: usize = 16;
/// The largest request that may wait for the caller; a larger deferred one is denied, so the
/// pending requests hold at most 16 MiB.
pub(crate) const MAX_PENDING_BYTES: usize = 1024 * 1024;

/// The length of `value` as JSON, without building the text.
pub(crate) fn json_len(value: &serde_json::Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}
const STDERR_DRAIN: Duration = Duration::from_millis(200);
/// The longest brief or follow-up a session takes.
pub(crate) const MAX_PROMPT_BYTES: usize = 1024 * 1024;

/// Checks a brief or follow-up: at most [`MAX_PROMPT_BYTES`], and not starting (after
/// whitespace) with `/`, which harnesses read as one of their slash commands.
pub(crate) fn check_prompt(text: &str) -> Result<(), LaunchError> {
    if text.len() > MAX_PROMPT_BYTES {
        return Err(LaunchError::PromptTooLarge(text.len()));
    }
    if text.trim_start().starts_with('/') {
        return Err(LaunchError::SlashCommandPrompt);
    }
    Ok(())
}

/// The exact process an adapter would start: its program, arguments, environment and working
/// directory. Built without side effects, so a caller can log or inspect a launch beforehand.
/// Its `Debug` output names the environment variables without their values, which hold
/// credentials, and counts the arguments that carry the MCP server's command line instead of
/// printing them, as they may hold one too.
#[derive(Clone, PartialEq, Eq)]
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
    /// How many of the last `args` describe the MCP server.
    pub(crate) server_args: usize,
}

impl std::fmt::Debug for CommandPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = self.args.len().saturating_sub(self.server_args);
        f.debug_struct("CommandPlan")
            .field("program", &self.program)
            .field("args", &&self.args[..shown])
            .field("mcp_server_args", &self.server_args)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// The pipes of a started harness, and the flag that tells their readers and writers to give
/// up (see [`crate::pipe`]).
pub(crate) struct ChildIo {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub abandon: Arc<AtomicBool>,
    /// What became of stdin once the adapter was done; the writer reports here.
    pub stdin_state: Arc<pipe::Stdin>,
}

/// Why a control message did not reach the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Undelivered {
    /// The driver's control queue is full.
    Full,
    /// The driver is gone.
    Closed,
}

/// How many control messages may wait for the driver.
pub(crate) const CONTROL_QUEUE: usize = 64;
/// The longest the watchdog waits before it looks at the wall clock again.
const WATCH_SLICE: Duration = Duration::from_secs(1);
/// How long dropping a session waits for its supervisor before leaving it to finish alone.
const DROP_WAIT: Duration = Duration::from_secs(2);

/// The protocol side of one session. `run` returns when the session's turns are over.
pub(crate) trait Driver: Send + 'static {
    fn run(self: Box<Self>, io: ChildIo, sink: EventSink) -> Result<(), LaunchError>;
}

/// Hands a control message to the driver without waiting.
pub(crate) type ControlTx = Box<dyn Fn(Control) -> Result<(), Undelivered> + Send + Sync>;

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
    /// The events on their way to the caller.
    events: Arc<EventQueue>,
    /// Set once the session is over or its group was killed: pipe I/O gives up.
    abandon: Arc<AtomicBool>,
    /// Set when the session is dropped: a retiring harness is killed without further grace.
    kill_now: AtomicBool,
    /// The approval requests left for the caller and not yet settled. Whatever is here when
    /// the session ends, however it ends, is reported withdrawn before `Finished`.
    pending: Mutex<Vec<String>>,
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
            events: Arc::default(),
            abandon: Arc::default(),
            kill_now: AtomicBool::new(false),
            pending: Mutex::default(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pending(&self) -> MutexGuard<'_, Vec<String>> {
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Where an adapter reports to. It also holds the two decisions an adapter must not make by
/// itself: whether a turn may begin, and what an approval request is answered with.
#[derive(Clone)]
pub(crate) struct EventSink {
    shared: Arc<Shared>,
}

impl EventSink {
    /// Queues `event` for the caller; it may wait while the queue is full.
    pub fn emit(&self, event: Event) {
        self.shared.events.push(event);
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
    /// On `Ask` the adapter keeps the request pending for the caller; with `pending` requests
    /// already waiting, [`MAX_PENDING_APPROVALS`] or more, it is denied instead.
    pub fn request_approval(&self, request: ApprovalRequest, pending: usize) -> ApprovalVerdict {
        self.request_approval_once(request, true, pending)
    }

    /// As [`Self::request_approval`], for a harness that may have no way to allow just once:
    /// without `can_allow_once` an `Allow` verdict becomes `Deny`.
    pub fn request_approval_once(
        &self,
        request: ApprovalRequest,
        can_allow_once: bool,
        pending: usize,
    ) -> ApprovalVerdict {
        let id = request.id.clone();
        let (mut verdict, mut source) = match &self.shared.policy {
            None => (ApprovalVerdict::Deny, ApprovalSource::DefaultDeny),
            Some(policy) => (policy.decide(&request), ApprovalSource::Policy),
        };
        if verdict == ApprovalVerdict::Allow && !can_allow_once {
            verdict = ApprovalVerdict::Deny;
        }
        if verdict == ApprovalVerdict::Ask
            && (pending >= MAX_PENDING_APPROVALS || json_len(&request.params) > MAX_PENDING_BYTES)
        {
            (verdict, source) = (ApprovalVerdict::Deny, ApprovalSource::TooManyPending);
        }
        self.emit(Event::ApprovalRequest(request));
        if verdict == ApprovalVerdict::Ask {
            self.shared.pending().push(id);
        } else {
            self.resolved(id, verdict == ApprovalVerdict::Allow, source);
        }
        verdict
    }

    pub fn resolved(&self, id: String, allowed: bool, source: ApprovalSource) {
        self.shared.pending().retain(|pending| *pending != id);
        self.emit(Event::ApprovalResolved { id, allowed, source });
    }

    #[cfg(test)]
    pub fn for_test(policy: Option<Arc<dyn ApprovalPolicy>>, max_turns: u32) -> (Self, Arc<EventQueue>) {
        let limits = Limits {
            max_turns,
            ..Limits::default()
        };
        let shared = Shared::new(0, limits, policy, Box::new(|_| Err(Undelivered::Closed)));
        let events = shared.events.clone();
        (Self { shared }, events)
    }
}

/// A running agent in another harness.
///
/// Events arrive in order and end with [`Event::Finished`]. Dropping the session kills the
/// harness's process group at once.
///
/// The launcher must not ignore SIGCHLD or set `SA_NOCLDWAIT`: the group is signalled while
/// its leader is known to be unreaped, which an automatic reap breaks. A launch is refused
/// in that state. Nor may it reap children it did not start (`waitpid(-1)`): a leader reaped
/// that way is not signalled again, so its group is left running until it exits. SIGPIPE
/// must be ignored, as it is in a Rust binary, or a write to an exited harness kills the
/// launcher.
pub struct Session {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    /// Disconnects when the supervisor thread ends.
    worker_done: Receiver<()>,
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
    ///
    /// Valid only until [`Event::Finished`]: the session reaps the process then, and the id
    /// may be reused by an unrelated process. Do not signal it after that.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// The next event, blocking until there is one. `None` once [`Event::Finished`] was read.
    #[must_use]
    pub fn recv(&self) -> Option<Event> {
        self.shared.events.recv_until(None).ok()
    }

    /// The next event, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// [`RecvTimeoutError::Timeout`] when none arrived in time, and
    /// [`RecvTimeoutError::Disconnected`] once [`Event::Finished`] was read.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, RecvTimeoutError> {
        self.shared.events.recv_timeout(timeout)
    }

    /// Every remaining event, blocking between them, ending after [`Event::Finished`].
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        std::iter::from_fn(|| self.recv())
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
    /// [`LaunchError::PromptTooLarge`] for a text over 1 MiB,
    /// [`LaunchError::SlashCommandPrompt`] for one starting with `/`, [`LaunchError::TurnLimit`] when
    /// the limits allow no further turn, and
    /// [`LaunchError::SessionClosed`] when the session has finished.
    pub fn send_follow_up(&self, text: impl Into<String>) -> Result<(), LaunchError> {
        let text = text.into();
        check_prompt(&text)?;
        let max = self.shared.limits.max_turns;
        self.shared
            .prompts
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |prompts| {
                (prompts < max).then_some(prompts + 1)
            })
            .map_err(|_| LaunchError::TurnLimit(max))?;
        self.send(Control::FollowUp(text))
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
        if self.shared.state().finished {
            return Err(LaunchError::SessionClosed);
        }
        match (self.shared.control)(control) {
            Ok(()) => Ok(()),
            Err(Undelivered::Full) => Err(LaunchError::ControlQueueFull),
            Err(Undelivered::Closed) => Err(LaunchError::SessionClosed),
        }
    }

    fn request_stop(&self, grace: Duration) {
        {
            let mut state = self.shared.state();
            if state.finished {
                return;
            }
            state.forced.get_or_insert(FinishStatus::Stopped);
            // `prepare` refused a grace that overflows; a stop without a deadline would still
            // end at the wall-time limit.
            if let Some(deadline) = Instant::now().checked_add(grace) {
                state.stop_deadline = Some(state.stop_deadline.map_or(deadline, |at| at.min(deadline)));
            }
        }
        self.shared.events.drain();
        self.shared.wake.notify_all();
        // Not delivered when the queue is full or the driver is gone: the watchdog still kills
        // the group at the stop deadline and then abandons the pipes.
        let _ = (self.shared.control)(Control::Stop);
    }
}

impl Drop for Session {
    /// Kills the process group at once and waits up to two seconds for the supervisor to
    /// reap it. A supervisor stuck beyond that (in a caller's approval policy, say) is left to
    /// finish on its own thread; the group is killed either way.
    fn drop(&mut self) {
        self.shared.kill_now.store(true, Ordering::SeqCst);
        self.request_stop(Duration::ZERO);
        self.shared.events.close();
        if let Some(worker) = self.worker.take()
            && !matches!(self.worker_done.recv_timeout(DROP_WAIT), Err(RecvTimeoutError::Timeout))
        {
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
    process::platform_supported().map_err(|feature| LaunchError::Unsupported {
        harness: spec.harness.kind.id(),
        feature,
    })?;
    #[cfg(unix)]
    if process::children_reaped_automatically() {
        return Err(LaunchError::Unsupported {
            harness: spec.harness.kind.id(),
            feature: "launching while SIGCHLD is ignored",
        });
    }
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
    #[cfg(unix)]
    process::verify_cwd(&mut command, &plan.cwd);
    #[cfg(unix)]
    let mut keeper = process::keep_group(&mut command)?;
    let spawned = spawn(&mut command);
    #[cfg(unix)]
    keeper.spawned();
    let mut child = spawned.map_err(|source| {
        #[cfg(unix)]
        if source.raw_os_error() == Some(process::CWD_CHANGED) {
            return LaunchError::WorkingDirChanged(plan.cwd.clone());
        }
        LaunchError::Spawn {
            executable: plan.program.clone(),
            source,
        }
    })?;
    let piped = "the child was spawned with piped stdio";
    let shared = Shared::new(child.id(), spec.limits, spec.approval_policy.clone(), control);
    let stdin_state = Arc::new(pipe::Stdin::default());
    let io = ChildIo {
        stdin_state: stdin_state.clone(),
        stdin: child.stdin.take().expect(piped),
        stdout: child.stdout.take().expect(piped),
        abandon: shared.abandon.clone(),
    };
    let mut stderr = pipe::Reader::new(child.stderr.take().expect(piped), shared.abandon.clone());
    let sink = EventSink { shared: shared.clone() };

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

    let (worker_alive, worker_done) = mpsc::channel::<()>();
    let worker = std::thread::spawn({
        let shared = shared.clone();
        move || {
            let _alive = worker_alive;
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
            shared.events.drain();
            // From here on only this thread signals the group: the watchdog saw `finished`.
            // A failed harness (a refused init frame, say) may be saving a refreshed credential,
            // so it is retired within the stop grace rather than killed outright.
            if matches!(
                status,
                FinishStatus::Completed | FinishStatus::Stopped | FinishStatus::Failed
            ) {
                let reading = stdin_closes(&stdin_state);
                // A stopped harness gets what is left of its stop grace, not the full waits.
                let budget = match status {
                    FinishStatus::Stopped => shared
                        .state()
                        .stop_deadline
                        .map_or(Duration::ZERO, |at| at.saturating_duration_since(Instant::now())),
                    FinishStatus::Failed => shared.limits.stop_grace,
                    _ => process::EXIT_GRACE + process::TERM_GRACE,
                };
                process::retire(&mut child, budget, &shared.kill_now, reading);
            } else {
                process::terminate(&mut child);
            }
            // The group is gone; the keeper's own kill now finds at most a descendant that a
            // foreign reaper left behind.
            #[cfg(unix)]
            drop(keeper);
            // A request the adapter did not settle (a harness that failed or exited mid-turn,
            // a panic) is moot once the group is gone.
            for id in std::mem::take(&mut *shared.pending()) {
                shared.events.push_last(Event::ApprovalResolved {
                    id,
                    allowed: false,
                    source: ApprovalSource::Withdrawn,
                });
            }
            if let (FinishStatus::Failed, Err(error)) = (status, &result) {
                let _ = stderr_drained.recv_timeout(STDERR_DRAIN);
                let tail = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let tail = String::from_utf8_lossy(&tail);
                let tail = tail.trim();
                let message = error.to_string();
                shared.events.push_last(Event::Diagnostics(if tail.is_empty() {
                    message.clone()
                } else {
                    format!("{message}\n{tail}")
                }));
                shared
                    .events
                    .push_last(Event::Error(crate::event::error_line(&message)));
            }
            shared.events.push_last(Event::Finished(status));
            shared.abandon.store(true, Ordering::SeqCst);
        }
    });

    Ok(Session {
        shared,
        worker: Some(worker),
        worker_done,
    })
}

/// Closes the harness's stdin now that the adapter is done with it, and whether the harness
/// was still reading it: a write that has waited for room since gives up, and a harness
/// that left it unread will not see the end of its input.
fn stdin_closes(stdin: &pipe::Stdin) -> bool {
    stdin.close.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + STDIN_CLOSE;
    while !stdin.closed.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    stdin.closed.load(Ordering::SeqCst) && !stdin.cut.load(Ordering::SeqCst)
}

/// How long the writer may take to give up a waiting write and close stdin: a few of its
/// poll slices.
const STDIN_CLOSE: Duration = Duration::from_millis(200);

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

/// Kills the process group at the wall-time deadline, or once a stop has had its grace. Wall
/// time is measured on [`process::wall_clock`], which counts time the machine slept, and the
/// wait is cut into slices so a wake from sleep is noticed.
fn watchdog(shared: &Shared, started: Instant) {
    // `prepare` refused a wall time that overflows; without a deadline only a stop ends it.
    let wall = started.checked_add(shared.limits.wall_time);
    let started_wall = process::wall_clock();
    let mut state = shared.state();
    loop {
        if state.finished {
            return;
        }
        let now = Instant::now();
        let stop_due = state.stop_deadline.is_some_and(|at| now >= at);
        let slept_through = started_wall
            .zip(process::wall_clock())
            .is_some_and(|(start, now)| now.saturating_sub(start) >= shared.limits.wall_time);
        let wall_due = slept_through || wall.is_some_and(|at| now >= at);
        if wall_due || stop_due {
            if !stop_due {
                state.forced.get_or_insert(FinishStatus::WallTimeExceeded);
            }
            // `finished` is false under the lock, so the leader is unreaped and the id is ours.
            process::kill_group(shared.pid);
            drop(state);
            shared.events.drain();
            // A descendant that left the group can hold the pipes open; the adapter is told
            // to stop and its pipe I/O gives up, instead of waiting for an end of output or
            // room in a pipe that never comes.
            shared.abandon.store(true, Ordering::SeqCst);
            let _ = (shared.control)(Control::Stop);
            return;
        }
        let until = match (state.stop_deadline, wall) {
            (Some(stop), Some(wall)) => Some(stop.min(wall)),
            (stop, wall) => stop.or(wall),
        };
        state = match until {
            Some(until) => {
                shared
                    .wake
                    .wait_timeout(state, (until - now).min(WATCH_SLICE))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            }
            None => shared
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        };
    }
}

#[cfg(all(test, unix))]
#[path = "session_tests.rs"]
mod tests;
