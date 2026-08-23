//! Agent-card retention helpers kept out of `store.rs` for the module-size cap.

use super::*;

impl CommsStore {
    /// Preview or apply every durable comms retention category except filesystem claim rows.
    pub fn cleanup(
        &self,
        apply: bool,
        message_ttl: std::time::Duration,
        thread_idle_ttl: std::time::Duration,
        thread_retention_ttl: std::time::Duration,
        agent_ttl: std::time::Duration,
    ) -> Result<crate::comms::protocol::CleanupReport, CommsStoreError> {
        let now = now_micros();
        let cutoff = |ttl: std::time::Duration| now.saturating_sub(i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX));
        let mut report = crate::comms::protocol::CleanupReport {
            applied: apply,
            ..Default::default()
        };
        let threads = self.list_threads()?;
        let archived: ahash::AHashSet<&ThreadId> = threads
            .iter()
            .filter(|thread| !thread.active)
            .map(|thread| &thread.id)
            .collect();
        let mut stale_agents = ahash::AHashSet::new();
        for guard in self.agents.iter() {
            let (_, value) = guard.into_inner()?;
            let record: AgentRecord = rmp_serde::from_slice(&value)?;
            let id = record.agent_id.as_str();
            if record.last_seen < cutoff(agent_ttl) && id.starts_with("session-") {
                stale_agents.insert(id.to_string());
            }
        }
        report.stale_agents = stale_agents.len();
        for guard in self.thread_members.iter() {
            let (key, _) = guard.into_inner()?;
            if keys::parse_thread_agent(&key).is_some_and(|(_, agent)| stale_agents.contains(&agent)) {
                report.stale_memberships += 1;
            }
        }
        for guard in self.cursors.iter() {
            let (key, _) = guard.into_inner()?;
            if keys::parse_cursor_key(&key).is_some_and(|(agent, _)| stale_agents.contains(&agent)) {
                report.stale_cursors += 1;
            }
        }
        for guard in self.messages_by_thread.iter() {
            let (_, value) = guard.into_inner()?;
            let meta: MessageMeta = rmp_serde::from_slice(&value)?;
            report.expired_messages +=
                usize::from(meta.ts_micros < cutoff(message_ttl) && !archived.contains(&meta.thread));
        }
        for thread in threads {
            let last = if thread.last_activity > 0 {
                thread.last_activity
            } else {
                thread.created_at
            };
            if thread.active && last < cutoff(thread_idle_ttl) {
                report.idle_threads += 1;
            }
            if last < cutoff(thread_retention_ttl)
                && (!thread.active || (thread.active && last < cutoff(thread_idle_ttl)))
            {
                report.archived_threads += 1;
            }
        }
        if apply {
            self.prune_expired(message_ttl)?;
            self.archive_idle(thread_idle_ttl)?;
            self.purge_archived(thread_retention_ttl)?;
            self.prune_ephemeral_agents(agent_ttl)?;
        }
        Ok(report)
    }

    /// Count active and stale generated agents using the supplied lifecycle threshold.
    pub fn agent_status(
        &self,
        ttl: std::time::Duration,
    ) -> Result<crate::comms::protocol::AgentsStatusReport, CommsStoreError> {
        let cutoff = now_micros().saturating_sub(i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX));
        let mut report = crate::comms::protocol::AgentsStatusReport::default();
        for guard in self.agents.iter() {
            let (_, value) = guard.into_inner()?;
            let record: AgentRecord = rmp_serde::from_slice(&value)?;
            let generated = record.agent_id.as_str().starts_with("session-");
            if generated && record.last_seen < cutoff {
                report.stale_agents += 1;
            } else {
                report.active_agents += 1;
            }
        }
        for thread in self.list_threads()? {
            if thread.active {
                report.active_threads += 1;
            } else {
                report.archived_threads += 1;
            }
        }
        report.last_maintenance_micros = self.last_maintenance()?;
        Ok(report)
    }

    /// Delete messages older than `now - ttl`, removing front-matter and bodies atomically.
    pub fn prune_expired(&self, ttl: std::time::Duration) -> Result<usize, CommsStoreError> {
        let ttl_micros = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
        let cutoff = now_micros().saturating_sub(ttl_micros);
        let archived: ahash::AHashSet<ThreadId> = self
            .list_threads()?
            .into_iter()
            .filter(|thread| !thread.active)
            .map(|thread| thread.id)
            .collect();
        let mut batch = self.db.batch();
        let mut pruned = 0usize;
        for guard in self.messages_by_thread.iter() {
            let (key, value) = guard.into_inner()?;
            let meta: MessageMeta = rmp_serde::from_slice(&value)?;
            if meta.ts_micros < cutoff && !archived.contains(&meta.thread) {
                batch.remove(&self.messages_by_thread, key.to_vec());
                batch.remove(&self.message_body, meta.id.as_bytes().to_vec());
                pruned += 1;
            }
        }
        if pruned > 0 {
            batch.commit()?;
        }
        Ok(pruned)
    }

    /// Archive active threads idle longer than `ttl`.
    pub fn archive_idle(&self, ttl: std::time::Duration) -> Result<usize, CommsStoreError> {
        let ttl_micros = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
        let cutoff = now_micros().saturating_sub(ttl_micros);
        let mut archived = 0usize;
        for thread in self.list_threads()? {
            let last = if thread.last_activity > 0 {
                thread.last_activity
            } else {
                thread.created_at
            };
            if thread.active && last < cutoff {
                let mut updated = thread;
                updated.active = false;
                self.put_thread(&updated)?;
                archived += 1;
            }
        }
        Ok(archived)
    }

    /// Permanently delete archived threads idle longer than `older_than` and all dependent rows.
    pub fn purge_archived(&self, older_than: std::time::Duration) -> Result<usize, CommsStoreError> {
        let ttl_micros = i64::try_from(older_than.as_micros()).unwrap_or(i64::MAX);
        let cutoff = now_micros().saturating_sub(ttl_micros);
        let doomed: Vec<ThreadId> = self
            .list_threads()?
            .into_iter()
            .filter(|thread| !thread.active)
            .filter(|thread| {
                let last = if thread.last_activity > 0 {
                    thread.last_activity
                } else {
                    thread.created_at
                };
                last < cutoff
            })
            .map(|thread| thread.id)
            .collect();
        if doomed.is_empty() {
            return Ok(0);
        }
        let doomed_set: ahash::AHashSet<&str> = doomed.iter().map(ThreadId::as_str).collect();
        let mut batch = self.db.batch();
        for guard in self.messages_by_thread.iter() {
            let (key, value) = guard.into_inner()?;
            let Some((thread, _)) = keys::parse_message_by_thread(&key) else {
                continue;
            };
            if doomed_set.contains(thread.as_str()) {
                let meta: MessageMeta = rmp_serde::from_slice(&value)?;
                batch.remove(&self.messages_by_thread, key.to_vec());
                batch.remove(&self.message_body, meta.id.as_bytes().to_vec());
            }
        }
        for guard in self.thread_members.iter() {
            let (key, _) = guard.into_inner()?;
            if let Some((thread, _)) = keys::parse_thread_agent(&key)
                && doomed_set.contains(thread.as_str())
            {
                batch.remove(&self.thread_members, key.to_vec());
                batch.remove(&self.thread_subs, key.to_vec());
            }
        }
        for guard in self.cursors.iter() {
            let (key, _) = guard.into_inner()?;
            if let Some((_, thread)) = keys::parse_cursor_key(&key)
                && doomed_set.contains(thread.as_str())
            {
                batch.remove(&self.cursors, key.to_vec());
            }
        }
        for thread in &doomed {
            batch.remove(&self.meta, keys::thread_seq_meta_key(thread.as_str()));
            batch.remove(&self.threads, keys::thread_key(thread.as_str()));
        }
        batch.commit()?;
        Ok(doomed.len())
    }

    /// Refresh an authenticated agent's activity timestamp at most once per `minimum_interval`.
    /// Returns `true` when a row was written.
    pub fn touch_agent_if_stale(
        &self,
        agent: &AgentId,
        minimum_interval: std::time::Duration,
    ) -> Result<bool, CommsStoreError> {
        let Some(mut record) = self.get_agent(agent)? else {
            return Ok(false);
        };
        let interval = i64::try_from(minimum_interval.as_micros()).unwrap_or(i64::MAX);
        let now = now_micros();
        if now.saturating_sub(record.last_seen) < interval {
            return Ok(false);
        }
        record.last_seen = now;
        self.put_agent(&record)?;
        Ok(true)
    }

    /// Remove generated agent cards whose last authenticated activity predates `ttl`.
    ///
    /// Authorship in message front matter is preserved, while roster, memberships, subscriptions,
    /// and read cursors are removed in one batch. Explicitly named agents are retained.
    pub fn prune_ephemeral_agents(&self, ttl: std::time::Duration) -> Result<usize, CommsStoreError> {
        let ttl_micros = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
        let cutoff = now_micros().saturating_sub(ttl_micros);
        let mut stale = ahash::AHashSet::new();
        for guard in self.agents.iter() {
            let (_, value) = guard.into_inner()?;
            let record: AgentRecord = rmp_serde::from_slice(&value)?;
            let id = record.agent_id.as_str();
            if record.last_seen < cutoff && id.starts_with("session-") {
                stale.insert(id.to_string());
            }
        }
        if stale.is_empty() {
            return Ok(0);
        }
        let mut batch = self.db.batch();
        for id in &stale {
            batch.remove(&self.agents, keys::agent_key(id));
        }
        for guard in self.thread_members.iter() {
            let (key, _) = guard.into_inner()?;
            if let Some((_, agent)) = keys::parse_thread_agent(&key)
                && stale.contains(&agent)
            {
                batch.remove(&self.thread_members, key.to_vec());
                batch.remove(&self.thread_subs, key.to_vec());
            }
        }
        for guard in self.cursors.iter() {
            let (key, _) = guard.into_inner()?;
            if let Some((agent, _)) = keys::parse_cursor_key(&key)
                && stale.contains(&agent)
            {
                batch.remove(&self.cursors, key.to_vec());
            }
        }
        for mut thread in self.list_threads()? {
            let before = thread.members.len();
            thread.members.retain(|member| !stale.contains(member.as_str()));
            if thread.members.len() != before {
                batch.insert(
                    &self.threads,
                    keys::thread_key(thread.id.as_str()),
                    rmp_serde::to_vec_named(&thread)?,
                );
            }
        }
        batch.commit()?;
        Ok(stale.len())
    }
}
