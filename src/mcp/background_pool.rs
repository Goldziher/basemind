//! The threads background builds run on: read-stack builds and term-index warms.
//!
//! Both are whole-corpus work that no request waits on directly. On the runtime's blocking pool,
//! which in the daemon also runs every agent request and forwarded read, a burst of them (a
//! reconnect storm, a watcher storm's warms across workspaces) took the threads those requests
//! needed. Here they queue for a few threads of their own, so interactive reads come first.

use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// Cores per read-stack build that may run at once.
const CORES_PER_BUILD: usize = 4;

/// Read-stack builds that may run at once, at most. Each holds an O(corpus) map while it runs, and
/// the daemon keeps three read stacks warm, so a fourth build at once would build a stack the warm
/// cap drops as soon as it lands.
const MAX_BUILD_SLOTS: usize = 3;

/// How many read-stack builds may run at once: one per [`CORES_PER_BUILD`] cores, at least one and
/// at most [`MAX_BUILD_SLOTS`]. The daemon's read-stack build gate holds this many permits.
pub(crate) fn build_slots() -> usize {
    build_slots_for(std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get))
}

fn build_slots_for(cores: usize) -> usize {
    (cores / CORES_PER_BUILD).clamp(1, MAX_BUILD_SLOTS)
}

/// Threads for background builds: one per build slot, and one more so a term warm runs while every
/// build slot is taken.
fn background_threads() -> usize {
    build_slots() + 1
}

type Job = Box<dyn FnOnce() + Send>;

/// A fixed set of named threads running queued jobs in order of arrival.
pub(crate) struct BackgroundPool {
    jobs: mpsc::Sender<Job>,
}

static BACKGROUND: LazyLock<BackgroundPool> = LazyLock::new(|| BackgroundPool::new(background_threads()));

/// The process's background pool.
pub(crate) fn background() -> &'static BackgroundPool {
    &BACKGROUND
}

impl BackgroundPool {
    fn new(threads: usize) -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let queue = Arc::new(Mutex::new(queue));
        for index in 0..threads {
            let queue = Arc::clone(&queue);
            std::thread::Builder::new()
                .name(format!("basemind-background-{index}"))
                .spawn(move || work(&queue))
                .expect("spawn a background pool thread");
        }
        Self { jobs }
    }

    /// Queue `job`. It runs inside the Tokio runtime the caller runs in, if any, so it can spawn
    /// tasks and blocking work of its own.
    pub(crate) fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        let runtime = tokio::runtime::Handle::try_current().ok();
        let job: Job = Box::new(move || {
            let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
            job();
        });
        self.jobs
            .send(job)
            .unwrap_or_else(|_| unreachable!("background pool threads outlive its sender"));
    }

    /// [`spawn`](Self::spawn) `job` and wait for its result without holding a thread. `None` when
    /// the job panicked.
    #[cfg(any(test, all(feature = "comms", any(unix, windows))))]
    pub(crate) async fn run<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (result, done) = tokio::sync::oneshot::channel();
        self.spawn(move || {
            // The caller may have stopped waiting; the job's work is done either way.
            let _ = result.send(job());
        });
        done.await.ok()
    }
}

/// One pool thread: run jobs until the pool's sender is dropped. A panicking job is logged and the
/// thread goes on with the next.
fn work(queue: &Mutex<mpsc::Receiver<Job>>) {
    loop {
        let next = queue.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok(job) = next else {
            return;
        };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
            tracing::error!("background build panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// Jobs past the pool's size wait for one of its threads, and a read on the runtime's blocking
    /// pool is answered while every background thread is busy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jobs_run_on_their_own_threads_at_most_the_pool_size_at_once() {
        let pool = Arc::new(BackgroundPool::new(2));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let jobs: Vec<_> = (0..5)
            .map(|index| {
                let (pool, running, peak, gate) = (
                    Arc::clone(&pool),
                    Arc::clone(&running),
                    Arc::clone(&peak),
                    Arc::clone(&gate),
                );
                tokio::spawn(async move {
                    pool.run(move || {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        let (open, opened) = &*gate;
                        let mut open = open.lock().expect("gate");
                        while !*open {
                            open = opened.wait(open).expect("gate");
                        }
                        running.fetch_sub(1, Ordering::SeqCst);
                        (index, std::thread::current().name().map(str::to_string))
                    })
                    .await
                })
            })
            .collect();
        for _ in 0..1000 {
            if running.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(running.load(Ordering::SeqCst), 2, "both threads hold a job");

        let read = tokio::time::timeout(Duration::from_secs(30), tokio::task::spawn_blocking(|| "answered")).await;
        assert_eq!(
            read.expect("the read waited behind the builds").expect("join"),
            "answered"
        );

        *gate.0.lock().expect("gate") = true;
        gate.1.notify_all();
        let mut done = Vec::new();
        for job in jobs {
            done.push(job.await.expect("join").expect("the job ran"));
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2, "at most the pool's size ran at once");
        assert_eq!(
            done.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(
            done.iter().all(|(_, name)| name
                .as_deref()
                .is_some_and(|name| name.starts_with("basemind-background-"))),
            "{done:?}"
        );
    }

    #[test]
    fn build_slots_grow_with_the_cores_within_their_bounds() {
        assert_eq!(
            [1, 2, 4, 7, 8, 12, 16, 64].map(build_slots_for),
            [1, 1, 1, 1, 2, 3, 3, 3]
        );
    }

    /// With a build in every build slot, a term warm still finds a thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_warm_runs_while_every_build_slot_is_taken() {
        let pool = Arc::new(BackgroundPool::new(background_threads()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let running = Arc::new(AtomicUsize::new(0));
        let builds: Vec<_> = (0..build_slots())
            .map(|_| {
                let (pool, release, running) = (Arc::clone(&pool), Arc::clone(&release), Arc::clone(&running));
                tokio::spawn(async move {
                    pool.run(move || {
                        running.fetch_add(1, Ordering::SeqCst);
                        let (open, opened) = &*release;
                        let mut open = open.lock().expect("release");
                        while !*open {
                            open = opened.wait(open).expect("release");
                        }
                    })
                    .await
                })
            })
            .collect();
        for _ in 0..1000 {
            if running.load(Ordering::SeqCst) == build_slots() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            running.load(Ordering::SeqCst),
            build_slots(),
            "every build slot holds a build"
        );

        let warm = tokio::time::timeout(Duration::from_secs(30), pool.run(|| "warmed")).await;
        assert_eq!(warm.expect("the warm waited behind the builds"), Some("warmed"));

        *release.0.lock().expect("release") = true;
        release.1.notify_all();
        for build in builds {
            assert_eq!(build.await.expect("join"), Some(()));
        }
    }

    /// A job that panics answers `None`, and its thread runs the next job.
    #[tokio::test]
    async fn a_panicking_job_leaves_its_thread_running() {
        let pool = BackgroundPool::new(1);
        assert_eq!(pool.run(|| -> u32 { panic!("a failed build") }).await, None);
        assert_eq!(pool.run(|| 7).await, Some(7));
    }
}
