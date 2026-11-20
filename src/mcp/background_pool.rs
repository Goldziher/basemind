//! The threads background builds run on: read-stack builds and term-index warms.
//!
//! Both are whole-corpus work that no request waits on directly. On the runtime's blocking pool,
//! which in the daemon also runs every agent request and forwarded read, a burst of them (a
//! reconnect storm, a watcher storm's warms across workspaces) took the threads those requests
//! needed. Here they queue for a few threads of their own, so interactive reads come first.

use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// Threads for background builds: a read-stack build (one at a time, by its gate) and a term warm
/// run side by side.
const BACKGROUND_THREADS: usize = 2;

type Job = Box<dyn FnOnce() + Send>;

/// A fixed set of named threads running queued jobs in order of arrival.
pub(crate) struct BackgroundPool {
    jobs: mpsc::Sender<Job>,
}

static BACKGROUND: LazyLock<BackgroundPool> = LazyLock::new(|| BackgroundPool::new(BACKGROUND_THREADS));

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

    /// A job that panics answers `None`, and its thread runs the next job.
    #[tokio::test]
    async fn a_panicking_job_leaves_its_thread_running() {
        let pool = BackgroundPool::new(1);
        assert_eq!(pool.run(|| -> u32 { panic!("a failed build") }).await, None);
        assert_eq!(pool.run(|| 7).await, Some(7));
    }
}
