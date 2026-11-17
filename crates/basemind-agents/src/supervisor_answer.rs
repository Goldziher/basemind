//! Answering a session's pending approval request: who may, and what is recorded before the
//! harness hears it.

use crate::audit::{AuditApproval, AuditEvent, AuditRecord};
use crate::error::LaunchError;
use crate::supervisor::{Resolved, SessionId, Supervisor, SupervisorError};
use crate::supervisor_admit::{Asker, owned, refuse};
use crate::supervisor_identity::resolve;
use crate::supervisor_types::Caller;

impl Supervisor {
    /// Allows (`allow`) or denies the pending approval request `approval` of session `id`.
    ///
    /// # Errors
    ///
    /// [`SupervisorError::UnknownSession`], [`SupervisorError::NotOwner`] unless `caller` is
    /// the operator or launched the session, [`SupervisorError::LauncherMayNotAllow`] for an
    /// allow by anyone but the operator unless the flavor's
    /// [`launcher_may_allow`](crate::FlavorPolicy::launcher_may_allow) is set,
    /// [`SupervisorError::Audit`] for an allow whose
    /// record could not be written (it is then not delivered), and the session's own errors (a
    /// finished session, a full control queue) as [`SupervisorError::Launch`].
    pub fn answer(&self, caller: &Caller, id: SessionId, approval: &str, allow: bool) -> Result<(), SupervisorError> {
        let inner = &self.inner;
        let mut table = inner.table();
        let resolved = resolve(&inner.policy, &table, caller);
        let asker = Asker::new(&table, caller, resolved.as_ref().ok());
        let operator = matches!(resolved, Ok(Resolved::Operator));
        let owned = owned(&mut table, resolved, id).and_then(|entry| {
            // An allow is the operator's, unless the flavor lets its launchers give one too.
            let launcher_may_allow = || {
                inner
                    .policy
                    .flavor(entry.flavor)
                    .is_some_and(|rule| rule.launcher_may_allow)
            };
            if allow && !operator && !launcher_may_allow() {
                Err(SupervisorError::LauncherMayNotAllow)
            } else {
                Ok(entry)
            }
        });
        let entry = match owned {
            Ok(entry) => entry,
            Err(error) => {
                drop(table);
                let mut record = AuditRecord {
                    session: Some(id.0),
                    ..AuditRecord::new(AuditEvent::ApprovalAnswer, error.code())
                };
                asker.stamp(&mut record);
                inner.refuse(record, &asker.key);
                return Err(error);
            }
        };
        // An id the session did not issue is the caller's own text: it is neither recorded nor
        // delivered. One already answered is refused the same way, so a request is allowed,
        // spent and recorded once; it is opened again only when its answer did not reach the
        // harness.
        let Some(pending) = entry.approvals.get_mut(approval).filter(|pending| !pending.answered) else {
            let flavor = entry.flavor.id();
            drop(table);
            let mut record = AuditRecord {
                session: Some(id.0),
                flavor: Some(flavor),
                ..AuditRecord::new(AuditEvent::ApprovalAnswer, "")
            };
            asker.stamp(&mut record);
            return Err(refuse(inner, record, &asker.key, SupervisorError::UnknownApproval));
        };
        pending.answered = true;
        let kind = pending.kind;
        let recorded_id = approval.to_owned();
        let handle = entry.handle.clone();
        let flavor = entry.flavor.id();
        let allowance = entry.allowance.clone();
        drop(table);
        let reopen = || {
            if let Some(pending) = inner
                .table()
                .sessions
                .get_mut(&id.0)
                .and_then(|entry| entry.approvals.get_mut(approval))
            {
                pending.answered = false;
            }
        };
        let record = |outcome, allowed| {
            let mut record = AuditRecord {
                session: Some(id.0),
                flavor: Some(flavor),
                approval: Some(AuditApproval {
                    id: recorded_id.clone(),
                    kind,
                    allowed,
                    source: "caller",
                }),
                ..AuditRecord::new(AuditEvent::ApprovalAnswer, outcome)
            };
            asker.stamp(&mut record);
            record
        };
        // An allow is on the record before it reaches the harness, or it does not reach it: the
        // request stays pending and is denied when its turn or the session ends. A denial
        // goes through whatever the log says.
        if allow {
            // An allow past the session's or the supervisor's allowance is refused, not
            // delivered: the request stays pending for a denial.
            if !allowance.take() {
                reopen();
                let error = SupervisorError::AllowsExhausted;
                inner.refuse(record(error.code(), false), &asker.key);
                return Err(error);
            }
            if let Err(error) = inner.audit(&record("allowed", true)) {
                reopen();
                return Err(SupervisorError::Audit(error));
            }
        }
        let answered = handle
            .ok_or(LaunchError::SessionClosed)
            .and_then(|handle| handle.answer(approval, allow))
            .map_err(SupervisorError::Launch);
        if answered.is_err() {
            reopen();
        }
        match (&answered, allow) {
            // The allow on the record did not reach the harness: that is kept on the record too,
            // never coalesced away. Its allowance stays spent, so each allow that is retried
            // spends another and the records stay within what the reserve holds per allow.
            (Err(_), true) => {
                let _ = inner.audit(&record("allow_undelivered", false));
            }
            // A denial or a failed one could be looped on: coalesced like a refusal.
            (Err(error), false) => inner.refuse(record(error.code(), false), &asker.key),
            (Ok(()), false) => inner.refuse(record("denied", false), &asker.key),
            (Ok(()), true) => {}
        }
        answered
    }
}
