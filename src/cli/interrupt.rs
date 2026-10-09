//! Ctrl-C handling for the one-shot scan commands.
//!
//! A scan commits per batch and checks its [`ScanCancel`] token once per file, so tripping the
//! token stops it cleanly: completed files stay committed, the stale-file purge is skipped (an
//! unscanned file is never mistaken for a deleted one), and the store lock is released by the
//! normal drop path. Killing the process instead would also be crash-safe, but it skips the
//! summary and leaves the lock sidecar behind until the OS reclaims the flock.
//!
//! The first Ctrl-C requests that clean stop; a second one means the user will not wait, so it
//! exits immediately with the conventional `130`.

use crate::scanner::ScanCancel;

use super::exit::INTERRUPTED;

/// Install the Ctrl-C handler and return the token it trips. The handler lives on a detached
/// thread for the rest of the process; this returns only once the signal is registered, so an
/// interrupt arriving right after the call is never lost to the default (immediate-kill) action.
pub fn install() -> ScanCancel {
    let cancel = ScanCancel::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let token = cancel.clone();
    let spawned = std::thread::Builder::new().name("ctrl-c".to_string()).spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            let _ = ready_tx.send(());
            return;
        };
        runtime.block_on(async move {
            let mut signal = match register() {
                Ok(signal) => signal,
                Err(error) => {
                    tracing::warn!(%error, "cannot install a Ctrl-C handler; an interrupt will kill the scan abruptly");
                    let _ = ready_tx.send(());
                    return;
                }
            };
            let _ = ready_tx.send(());
            signal.recv().await;
            token.cancel();
            eprintln!("interrupted: stopping after the files in flight (press Ctrl-C again to abort now)");
            signal.recv().await;
            std::process::exit(i32::from(INTERRUPTED));
        });
    });
    if spawned.is_ok() {
        let _ = ready_rx.recv();
    }
    cancel
}

#[cfg(unix)]
fn register() -> std::io::Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
}

#[cfg(windows)]
fn register() -> std::io::Result<tokio::signal::windows::CtrlC> {
    tokio::signal::windows::ctrl_c()
}
