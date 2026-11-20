//! The scan slots bound every daemon scan, and a hosted workspace's watcher batches wait for one
//! merged, without holding a thread.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;

use super::{drain, on_blocking_slot};
use crate::engine::scan_queue::{QueuedPass, ScanQueue};
use crate::engine::types::index::QueuedScan;

/// Rescans past the bound wait for a slot instead of taking more blocking threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescans_past_the_bound_wait_for_a_slot() {
    static SLOTS: Semaphore = Semaphore::const_new(2);
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    let tasks: Vec<_> = (0..6)
        .map(|_| {
            tokio::spawn(on_blocking_slot(&SLOTS, || {
                let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
                PEAK.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                RUNNING.fetch_sub(1, Ordering::SeqCst);
            }))
        })
        .collect();
    for task in tasks {
        task.await.expect("join").expect("blocking work");
    }
    assert_eq!(PEAK.load(Ordering::SeqCst), 2, "at most the slot count run at once");
}

/// Shut until the test opens it: a pass waiting on it holds its slot.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().expect("gate");
        while !*open {
            open = self.opened.wait(open).expect("gate");
        }
    }

    fn open(&self) {
        *self.open.lock().expect("gate") = true;
        self.opened.notify_all();
    }
}

/// What the passes of a storm did: how many ran at once, and each pass with its workspace.
#[derive(Default)]
struct Passes {
    running: AtomicUsize,
    peak: AtomicUsize,
    ran: Mutex<Vec<(usize, QueuedPass)>>,
}

impl Passes {
    fn ran(&self) -> usize {
        self.ran.lock().expect("passes").len()
    }
}

/// One workspace's watcher batch naming `names`.
fn batch(workspace: usize, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| PathBuf::from(format!("w{workspace}/{name}.rs")))
        .collect()
}

/// Start the drain of workspace `id`'s queue, as the serve watcher does for its first batch.
fn start_drain(
    slots: &'static Semaphore,
    id: usize,
    queue: &Arc<ScanQueue>,
    passes: &Arc<Passes>,
    gate: &Arc<Gate>,
) -> tokio::task::JoinHandle<()> {
    let (queue, passes, gate) = (Arc::clone(queue), Arc::clone(passes), Arc::clone(gate));
    tokio::spawn(drain(
        slots,
        move || queue.take(),
        move |pass| {
            let now = passes.running.fetch_add(1, Ordering::SeqCst) + 1;
            passes.peak.fetch_max(now, Ordering::SeqCst);
            gate.wait();
            passes.ran.lock().expect("passes").push((id, pass));
            passes.running.fetch_sub(1, Ordering::SeqCst);
        },
    ))
}

/// Ten workspaces' storms in one: one batch each starts a drain, and two more batches each arrive
/// while the first passes run. At most the slot count of passes ever run, a workspace still waiting
/// for a slot scans all three batches in one pass, and one whose first pass held a slot scans the
/// two later ones in one more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storm_over_ten_workspaces_runs_at_most_the_slot_count_and_merges_later_batches() {
    static SLOTS: Semaphore = Semaphore::const_new(4);
    let (passes, gate) = (Arc::new(Passes::default()), Arc::new(Gate::default()));
    let queues: Vec<Arc<ScanQueue>> = (0..10).map(|_| Arc::default()).collect();
    let mut drains = Vec::new();
    for (id, queue) in queues.iter().enumerate() {
        assert!(queue.submit(Some(batch(id, &["a"]))), "workspace {id}: no drain yet");
        drains.push(start_drain(&SLOTS, id, queue, &passes, &gate));
    }
    for _ in 0..1000 {
        if passes.running.load(Ordering::SeqCst) == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(passes.running.load(Ordering::SeqCst), 4, "every slot holds a pass");

    for (id, queue) in queues.iter().enumerate() {
        assert!(
            !queue.submit(Some(batch(id, &["b"]))),
            "workspace {id}: the drain owns the queue"
        );
        assert!(
            !queue.submit(Some(batch(id, &["c"]))),
            "workspace {id}: the drain owns the queue"
        );
    }
    let waiting: Vec<QueuedScan> = queues.iter().filter_map(|queue| queue.queued()).collect();
    let count = |batches, paths| {
        let shape = QueuedScan {
            batches,
            paths,
            whole_tree: false,
        };
        waiting.iter().filter(|queued| **queued == shape).count()
    };
    assert_eq!((count(2, 2), count(3, 3)), (4, 6), "{waiting:?}");

    gate.open();
    for handle in drains {
        handle.await.expect("drain");
    }
    assert_eq!(
        passes.peak.load(Ordering::SeqCst),
        4,
        "at most the slot count ran at once"
    );
    let ran = passes.ran.lock().expect("passes");
    assert_eq!(
        ran.len(),
        4 * 2 + 6,
        "one pass per waiting workspace, two per running one"
    );
    for id in 0..10 {
        let mine: Vec<&QueuedPass> = ran.iter().filter(|(of, _)| *of == id).map(|(_, pass)| pass).collect();
        let held_a_slot = [
            QueuedPass::Paths(batch(id, &["a"])),
            QueuedPass::Paths(batch(id, &["b", "c"])),
        ];
        let waited = [QueuedPass::Paths(batch(id, &["a", "b", "c"]))];
        assert!(
            mine.iter().copied().eq(held_a_slot.iter()) || mine.iter().copied().eq(waited.iter()),
            "workspace {id}: {mine:?}"
        );
    }
}

/// With every slot taken and ten drains waiting, a read on a blocking pool of ONE thread is still
/// answered: a drain waits for its slot without holding a thread, and runs no pass until it has one.
#[test]
fn a_read_during_a_storm_is_answered_while_every_slot_is_taken() {
    static SLOTS: Semaphore = Semaphore::const_new(2);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let held = SLOTS.acquire_many(2).await.expect("slots");
        let (passes, gate) = (Arc::new(Passes::default()), Arc::new(Gate::default()));
        gate.open();
        let queues: Vec<Arc<ScanQueue>> = (0..10).map(|_| Arc::default()).collect();
        let drains: Vec<_> = queues
            .iter()
            .enumerate()
            .map(|(id, queue)| {
                assert!(queue.submit(Some(batch(id, &["a"]))));
                start_drain(&SLOTS, id, queue, &passes, &gate)
            })
            .collect();
        // One turn of the single-threaded scheduler runs every drain up to its wait for a slot.
        tokio::task::yield_now().await;

        let read = tokio::time::timeout(Duration::from_secs(30), tokio::task::spawn_blocking(|| "answered")).await;
        assert_eq!(
            read.expect("the read waited behind the scans").expect("join"),
            "answered"
        );
        assert_eq!(passes.ran(), 0, "no pass runs without a slot");
        assert_eq!(queues.iter().filter(|queue| queue.queued().is_some()).count(), 10);

        drop(held);
        for handle in drains {
            handle.await.expect("drain");
        }
        assert_eq!(passes.ran(), 10);
    });
}
