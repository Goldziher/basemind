//! The thread that owns a running session, and the slot a launch holds until that thread
//! takes the session over.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::audit::{AuditApproval, AuditEvent, AuditRecord};
use crate::detached::{self, Detached};
use crate::event::{Event, FinishStatus};
use crate::harness::HarnessKind;
use crate::policy::Caveat;
use crate::session::Session;
use crate::supervisor::{Inner, SessionId, SessionState};
use crate::supervisor_codes::{approval_kind, approval_source, finish_code};

/// The most approval requests whose kind a session remembers until they are settled.
const MAX_REMEMBERED_APPROVALS: usize = 256;

/// A session's slot in the table, taken at admission and not yet handed to its pump. Dropped
/// before [`Reservation::disarm`] (an adapter error, a failed thread spawn, a panic in the
/// adapter), it gives the slot and the launch back and records the refusal.
pub(crate) struct Reservation<'a> {
    inner: &'a Inner,
    id: u64,
    refusal: AuditRecord,
    /// The refusal's outcome when the slot is given back.
    pub outcome: &'static str,
    armed: bool,
}

impl<'a> Reservation<'a> {
    pub fn new(inner: &'a Inner, id: u64, refusal: AuditRecord) -> Self {
        Self {
            inner,
            id,
            refusal,
            outcome: "launch_panicked",
            armed: true,
        }
    }

    /// The pump owns the session now.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut table = self.inner.table();
        if table.sessions.remove(&self.id).is_some() {
            table.launches = table.launches.saturating_sub(1);
        }
        drop(table);
        self.inner.changed.notify_all();
        let key = self.refusal.caller.clone().unwrap_or_default();
        self.inner.refuse(
            AuditRecord {
                session: Some(self.id),
                outcome: self.outcome,
                ..self.refusal.clone()
            },
            &key,
        );
    }
}

/// Marks the session finished if its pump unwinds before it could, so the session's slot is
/// not held by an entry nothing will ever update.
struct Finish<'a> {
    inner: &'a Inner,
    id: u64,
    done: bool,
}

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(entry) = self.inner.table().sessions.get_mut(&self.id) {
            entry.state = SessionState::Finished(FinishStatus::Failed);
            entry.handle = None;
            entry.leader = None;
        }
        self.inner.changed.notify_all();
    }
}

/// The thread that owns one session: it reads every event, hands it to the observer, audits
/// each settled approval, records the harness's detached processes for a flavor that makes
/// them, and once the session has finished, reaps those processes, marks the session finished
/// and writes its exit record. An observer that panics is dropped for the session and the
/// session is stopped; the events are still read to the end.
pub(crate) fn supervise(inner: &Inner, id: u64, session: Session, detached: &Arc<Detached>, flavor: HarnessKind) {
    let mut finish = Finish { inner, id, done: false };
    let tracks = crate::policy::caveats(flavor).contains(&Caveat::DetachedServer);
    let done = Arc::new(AtomicBool::new(false));
    let tracker = tracks
        .then(|| {
            let (detached, done) = (detached.clone(), done.clone());
            std::thread::Builder::new()
                .name(format!("basemind-detached-{id}"))
                .spawn(move || {
                    // Once the leader is gone its pid may name another process: stop polling.
                    while !done.load(Ordering::SeqCst) && detached.poll() {
                        std::thread::sleep(detached::POLL);
                    }
                })
                .ok()
        })
        .flatten();
    if tracks && tracker.is_none() {
        // Its detached processes could not be followed: the session does not run unwatched.
        session.stop();
    }
    let mut observer = inner.observer.get();
    let mut status = FinishStatus::Failed;
    for event in session.events() {
        if let Some(watching) = observer {
            let delivered = std::panic::catch_unwind(AssertUnwindSafe(|| watching.event(SessionId(id), &event)));
            if delivered.is_err() {
                observer = None;
                session.stop();
            }
        }
        match &event {
            Event::ApprovalRequest(request) => {
                let mut table = inner.table();
                if let Some(entry) = table.sessions.get_mut(&id)
                    && entry.approvals.len() < MAX_REMEMBERED_APPROVALS
                {
                    entry.approvals.insert(request.id.clone(), approval_kind(request.kind));
                }
            }
            Event::ApprovalResolved {
                id: approval,
                allowed,
                source,
            } => {
                let kind = inner
                    .table()
                    .sessions
                    .get_mut(&id)
                    .and_then(|entry| entry.approvals.remove(approval))
                    .unwrap_or("unknown");
                let recorded = inner.audit(&AuditRecord {
                    session: Some(id),
                    flavor: Some(flavor.id()),
                    approval: Some(AuditApproval {
                        id: approval.clone(),
                        kind,
                        allowed: *allowed,
                        source: approval_source(*source),
                    }),
                    ..AuditRecord::new(AuditEvent::Approval, if *allowed { "allowed" } else { "denied" })
                });
                // The harness already has the allow; an allow that is not on the record ends
                // the session rather than let it act on it unrecorded.
                if *allowed && recorded.is_err() {
                    session.stop();
                }
            }
            Event::Finished(finished) => status = *finished,
            _ => {}
        }
    }
    done.store(true, Ordering::SeqCst);
    if let Some(tracker) = tracker {
        let _ = tracker.join();
    }
    let killed = if tracks { detached.reap(detached::EXIT_GRACE) } else { 0 };
    drop(session);
    let mut table = inner.table();
    let record = table.sessions.get_mut(&id).map(|entry| {
        entry.state = SessionState::Finished(status);
        entry.handle = None;
        entry.leader = None;
        AuditRecord {
            session: Some(id),
            agent: Some(entry.agent.clone()),
            flavor: Some(flavor.id()),
            depth: Some(entry.depth),
            caveats: if killed > 0 {
                vec![Caveat::DetachedServer.id()]
            } else {
                Vec::new()
            },
            ..AuditRecord::new(AuditEvent::Exit, finish_code(status))
        }
    });
    finish.done = true;
    drop(table);
    inner.changed.notify_all();
    if let Some(record) = record {
        let _ = inner.audit(&record);
    }
}
