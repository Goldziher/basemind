//! Off-reactor execution of the machine-registry calls of the [`Broker`](super::Broker): its
//! mutations rewrite a file and its prune stats every row's path, so none of that belongs on an
//! async worker.

use std::sync::Arc;

use super::*;

impl Broker {
    /// Run `f` against the machine registry on the blocking pool. Its mutations rewrite a file and
    /// its prune stats every row's path, so none of that belongs on a worker. The registry's async
    /// lock is taken owned and moved into the closure: other registry users queue behind it (that
    /// is the lock's job) but no runtime worker is parked while it is held.
    pub(crate) async fn registry_blocking<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(&mut MachineRegistry) -> T + Send + 'static,
    {
        let mut guard = Arc::clone(&self.machine_registry).lock_owned().await;
        match tokio::task::spawn_blocking(move || f(&mut guard)).await {
            Ok(value) => value,
            Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
            // Only a runtime shutdown cancels a blocking task; the caller is being torn down too.
            Err(_) => std::future::pending().await,
        }
    }
}
