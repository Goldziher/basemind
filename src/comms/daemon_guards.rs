//! RAII refcount guards over [`Broker`]: one per connected front-end link, one per client-less
//! work unit. Split out of `daemon.rs` to keep that file under the module-size cap; the invariants
//! they enforce live on [`Broker::register_link`] and [`Broker::begin_work`].

use std::sync::Arc;

use super::daemon::Broker;

/// RAII refcount for one connected link, held for the link's whole life; see
/// [`Broker::register_link`].
///
/// The guard is taken in the ACCEPT LOOP, synchronously, before the link is spawned onto its own
/// task — not inside that task. That ordering is the point: if the increment happened inside the
/// spawned task, there would be a window in which a connection had been accepted but was not yet
/// counted, and an idle check landing in that window would see zero links and reap a daemon that
/// had just taken on work.
pub struct LinkGuard {
    pub(super) broker: Arc<Broker>,
}

impl Drop for LinkGuard {
    fn drop(&mut self) {
        self.broker.link_disconnected();
    }
}

/// RAII marker that a unit of daemon-internal work is in flight; see [`Broker::begin_work`].
///
/// Work with a client attached is already covered by the link refcount — the client blocks on the
/// socket for the whole RPC. This exists for the work that has NO client: the periodic
/// cross-workspace blob GC, which the reaper must not tear down mid-sweep (it is the sole writer of
/// the global blob store, and a half-applied sweep is exactly the torn state the reap is supposed to
/// avoid). Dropping the guard stamps activity, so a sweep that finishes also restarts the idle clock.
pub struct WorkGuard<'a> {
    pub(super) broker: &'a Broker,
}

impl Drop for WorkGuard<'_> {
    fn drop(&mut self) {
        self.broker.end_work();
    }
}
