use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;

use super::{new_build_gate, run_gated_build};

#[tokio::test]
async fn gated_build_keeps_its_permit_until_the_build_ends_even_if_the_caller_is_dropped() {
    let gate = Arc::new(Semaphore::new(1));
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();

    let first = {
        let (running, peak) = (running.clone(), peak.clone());
        tokio::spawn(run_gated_build(gate.clone(), Duration::from_secs(5), move || {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            running.fetch_sub(1, Ordering::SeqCst);
        }))
    };
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    // The caller goes away mid-build (client disconnect / timeout / drain).
    first.abort();
    let _ = first.await;

    let second = {
        let (running, peak) = (running.clone(), peak.clone());
        tokio::spawn(run_gated_build(gate.clone(), Duration::from_secs(5), move || {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            running.fetch_sub(1, Ordering::SeqCst);
        }))
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !second.is_finished(),
        "second build must wait for the orphaned first build"
    );
    release_tx.send(()).unwrap();
    second.await.unwrap().expect("second build runs after the first ends");
    assert_eq!(peak.load(Ordering::SeqCst), 1, "builds never overlapped");
}

#[tokio::test]
async fn gated_build_times_out_instead_of_waiting_forever() {
    let gate = Arc::new(Semaphore::new(1));
    let _held = gate.clone().acquire_owned().await.unwrap();
    let error = run_gated_build(gate, Duration::from_millis(50), || ())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
}

/// The daemon's gate runs as many builds at once as it has build slots, and a build past them
/// waits for one to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_build_gate_runs_its_build_slots_at_once_and_queues_the_rest() {
    let slots = crate::mcp::background_pool::build_slots();
    let gate = new_build_gate();
    assert_eq!(gate.available_permits(), slots);
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let builds: Vec<_> = (0..=slots)
        .map(|_| {
            let (running, peak, release) = (running.clone(), peak.clone(), release.clone());
            tokio::spawn(run_gated_build(gate.clone(), Duration::from_secs(30), move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let (open, opened) = &*release;
                let mut open = open.lock().expect("release");
                while !*open {
                    open = opened.wait(open).expect("release");
                }
                running.fetch_sub(1, Ordering::SeqCst);
            }))
        })
        .collect();
    for _ in 0..1000 {
        if running.load(Ordering::SeqCst) == slots {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(running.load(Ordering::SeqCst), slots, "every build slot holds a build");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        (running.load(Ordering::SeqCst), gate.available_permits()),
        (slots, 0),
        "the build past the slots waits at the gate"
    );

    *release.0.lock().expect("release") = true;
    release.1.notify_all();
    for build in builds {
        build.await.expect("join").expect("the build ran");
    }
    assert_eq!(
        peak.load(Ordering::SeqCst),
        slots,
        "at most the build slots ran at once"
    );
}
