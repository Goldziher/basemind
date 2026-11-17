//! Binding launchers and the operator to processes: the in-process and held-child
//! registrations, their release, and the in-process operator.

use std::sync::atomic::Ordering;

use crate::audit::{AuditEvent, AuditRecord};
use crate::peer::Peer;
use crate::supervisor::{Supervisor, SupervisorError};
use crate::supervisor_admit::refuse;
use crate::supervisor_identity::{Role, Source, mint_operator, register, role_name};
use crate::supervisor_reserve::MAX_REGISTRATIONS;
use crate::supervisor_types::Caller;

impl Supervisor {
    /// Binds the configured launcher `principal` to `child`, a process the embedding spawned
    /// itself and has not waited for, such as an operator client it starts. Register it right
    /// after the spawn: a child that has already exited is refused, since its pid could name
    /// another process once it is reaped. From then on a caller is `principal` only when its
    /// peer is that process (pid and start time) or descends from it; until a principal is
    /// registered, no caller is it.
    ///
    /// Everything the child starts acts as the principal, so it must start nothing on another
    /// agent's request and run nothing another agent can write (no shell tool, hooks, build
    /// steps or scripts from a tree a launched agent edits): an agent lead does not qualify;
    /// host one in its own process with [`Self::register_launcher_in_process`] instead. A
    /// binding is write-once: it is released only by [`Self::unregister_launcher`], also once
    /// its process is gone.
    ///
    /// # Errors
    ///
    /// [`SupervisorError::CallerNotAdmitted`] for a principal the policy does not name, or a
    /// process that is not this process's child or is a session's;
    /// [`SupervisorError::PeerUnverified`] for a child that has exited or cannot be read;
    /// [`SupervisorError::RoleBound`] while the principal is bound;
    /// [`SupervisorError::Audit`] when the registration could not be recorded (it is then
    /// undone). Each is audited.
    pub fn register_launcher_child(
        &self,
        principal: &str,
        child: &mut std::process::Child,
    ) -> Result<(), SupervisorError> {
        // An unreaped child keeps its pid, so the pid read next is this child's or a zombie's,
        // which is refused.
        let source = match child.try_wait() {
            Ok(None) => Source::Child(child.id()),
            Ok(Some(_)) | Err(_) => Source::Exited(child.id()),
        };
        self.bind(Role::Launcher(principal.to_owned()), source)
    }

    /// Binds the configured launcher `principal` to this process: the topology the crate
    /// supports for an agent lead, whose own `basemind serve` (or an in-process embedding)
    /// hosts the supervisor and calls it for the lead's tool calls. Only this process is the
    /// principal then, never what descends from it.
    ///
    /// # Errors
    ///
    /// As [`Self::register_launcher_child`].
    pub fn register_launcher_in_process(&self, principal: &str) -> Result<(), SupervisorError> {
        self.bind(Role::Launcher(principal.to_owned()), Source::InProcess)
    }

    /// Binds the operator to this process, after which [`Self::operator`] mints it. The
    /// operator exists in-process only: the embedding mints it for a decision that came from its
    /// own channel to the human (such as an MCP elicitation answered in the lead's client),
    /// never for a tool call or a connection a launched agent can make.
    ///
    /// The operator and an in-process launcher share this process. That admits nothing more:
    /// an in-process binding admits no descendant, and only [`Self::operator`] makes the
    /// operator caller.
    ///
    /// # Errors
    ///
    /// As [`Self::register_launcher_child`], but for the operator.
    pub fn register_operator_in_process(&self) -> Result<(), SupervisorError> {
        self.bind(Role::Operator, Source::InProcess)
    }

    /// Releases the binding of the configured launcher `principal`, the only way to rebind it.
    /// Returns whether one was released. Callers it admitted, already made, are refused from
    /// then on. The release is recorded; a record that cannot be written is counted
    /// ([`Self::audit_failures`]) and the binding is released all the same.
    pub fn unregister_launcher(&self, principal: &str) -> bool {
        self.unbind(&Role::Launcher(principal.to_owned()))
    }

    /// Releases the operator's binding, as [`Self::unregister_launcher`] a launcher's: an
    /// operator caller minted before is refused from then on.
    pub fn unregister_operator(&self) -> bool {
        self.unbind(&Role::Operator)
    }

    fn bind(&self, role: Role, source: Source) -> Result<(), SupervisorError> {
        let inner = &self.inner;
        // Held until the record is written, so a failed write undoes only its own binding.
        let _registering = inner
            .registering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let caller = role_name(&role);
        let key = format!("register-{caller}");
        let mut table = inner.table();
        let bound = if inner.registrations.load(Ordering::SeqCst) >= MAX_REGISTRATIONS {
            Err(SupervisorError::RegistrationLimit)
        } else {
            register(&inner.policy, &mut table, role.clone(), source)
        };
        drop(table);
        let mut record = AuditRecord {
            caller: Some(caller),
            ..AuditRecord::new(AuditEvent::Registration, "registered")
        };
        let id = match bound {
            Ok(id) => id,
            Err(error) => {
                record.peer = Some(source.pid());
                return Err(refuse(inner, record, &key, error));
            }
        };
        record.peer = Some(id.pid.unsigned_abs());
        inner.registrations.fetch_add(1, Ordering::SeqCst);
        if let Err(error) = inner.audit(&record) {
            inner.table().registered.retain(|entry| *entry != (role.clone(), id));
            return Err(SupervisorError::Audit(error));
        }
        Ok(())
    }

    fn unbind(&self, role: &Role) -> bool {
        let inner = &self.inner;
        let _registering = inner
            .registering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut table = inner.table();
        let released = table
            .registered
            .iter()
            .find(|(bound, _)| bound == role)
            .map(|(_, id)| *id);
        table.registered.retain(|(bound, _)| bound != role);
        drop(table);
        let Some(id) = released else {
            return false;
        };
        // A release is always made; past the limit it is only counted, not recorded.
        if inner.registrations.fetch_add(1, Ordering::SeqCst) >= MAX_REGISTRATIONS {
            inner.audit_failures.fetch_add(1, Ordering::SeqCst);
            return true;
        }
        let _ = inner.audit(&AuditRecord {
            caller: Some(role_name(role)),
            peer: Some(id.pid.unsigned_abs()),
            ..AuditRecord::new(AuditEvent::Registration, "unregistered")
        });
        true
    }

    /// The human operator: may stop and answer any session and launches nothing. Made only
    /// here, in this process, once [`Self::register_operator_in_process`] bound it; there is no
    /// operator for a connection. The embedding calls this only for a decision the human made
    /// through its own channel, never on a launched agent's request. The binding is checked
    /// again on every call the operator makes.
    ///
    /// # Errors
    ///
    /// [`SupervisorError::CallerNotAdmitted`] while no operator is bound;
    /// [`SupervisorError::PeerUnverified`] where this process cannot be read. Each refusal is
    /// audited.
    pub fn operator(&self) -> Result<Caller, SupervisorError> {
        let minted = Peer::embedding()
            .map_err(|_| SupervisorError::PeerUnverified)
            .and_then(|peer| mint_operator(&self.inner.table(), peer));
        if let Err(error) = &minted {
            self.inner.refuse(
                AuditRecord {
                    claimed: Some("operator".to_owned()),
                    peer: Some(std::process::id()),
                    ..AuditRecord::new(AuditEvent::Refusal, error.code())
                },
                "supervisor",
            );
        }
        minted
    }
}
