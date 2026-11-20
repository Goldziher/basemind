//! The daemon-hosted read stacks of the [`WorkspacePool`](super::WorkspacePool): building, sharing,
//! trimming and shedding each hot workspace's read stack, and the async open that serves it. Split
//! out of `workspace_pool.rs` to keep it under the 1000-line `rust-max-lines` cap; this is a second
//! `impl WorkspacePool` block.

use super::*;

impl WorkspacePool {
    /// Build (or fetch) the daemon-hosted shared read stack for `root`, opening the workspace into
    /// the pool if cold. The first caller builds it on a blocking thread ([`Self::host_read_stack`])
    /// and starts the workspace's single freshness warden; every later caller gets the same `Arc`.
    /// Concurrent callers all await the one build, because the slot lock is held across it.
    ///
    /// `git_history` yields the git-history index of a root the daemon holds, and the pin its syncs
    /// hold, for a workspace in a git repository; it runs only when a stack is built.
    ///
    /// Building one stack is what enforces the warm-stack cap: the trim runs here, AFTER the slot
    /// lock is released, so admitting a new stack is the moment the least-recently-used ones are
    /// shed.
    #[cfg(test)]
    pub(crate) async fn get_or_build_serve_state(
        self: &std::sync::Arc<Self>,
        root: &Path,
        git_history: impl FnOnce(&Path) -> Option<crate::comms::git_history_ops::HostedHistory> + Send + 'static,
    ) -> anyhow::Result<crate::mcp::ServedWorkspace> {
        let (shared, _conn) = self.serve_conn(root, git_history).await?;
        Ok(shared)
    }

    /// [`Self::get_or_build_serve_state`] for a connection that will be served from the stack: the
    /// connection is registered on the entry before the slot lock is released, so no eviction or
    /// trim can drop the stack between handing it out and the session pinning it. With the two as
    /// separate steps, a trim in that gap left the session on an orphaned stack whose watcher had
    /// stopped, and the next session built a second one.
    pub(crate) async fn serve_conn(
        self: &std::sync::Arc<Self>,
        root: &Path,
        git_history: impl FnOnce(&Path) -> Option<crate::comms::git_history_ops::HostedHistory> + Send + 'static,
    ) -> anyhow::Result<(crate::mcp::ServedWorkspace, ServeConnGuard)> {
        let (entry, conn) = loop {
            let entry = self.get_or_open_async(root).await.map_err(anyhow::Error::new)?;
            let conn = Self::register_conn(std::sync::Arc::clone(&entry));
            // Evicted between the open and the pin: open it again rather than serve a detached entry.
            if self
                .lock_map()
                .get(&entry.key)
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, &entry))
            {
                break (entry, conn);
            }
        };
        let (shared, built, conn) = {
            let mut slot = entry.serve_state.lock().await;
            match slot.as_ref() {
                Some(hosted) => (hosted.shared(), false, conn),
                None => {
                    let (pool, building, root_buf) = (
                        std::sync::Arc::clone(self),
                        std::sync::Arc::clone(&entry),
                        root.to_path_buf(),
                    );
                    let hosted = super::read_stack_gate::run_gated_build(
                        std::sync::Arc::clone(&super::read_stack_gate::READ_STACK_BUILD_GATE),
                        super::read_stack_gate::READ_STACK_BUILD_GATE_WAIT,
                        move || pool.host_read_stack(&building, &root_buf, git_history),
                    )
                    .await??;
                    let shared = hosted.shared();
                    *slot = Some(hosted);
                    (shared, true, conn)
                }
            }
        };
        // Only an admission can push the warm set over the cap, and the stateless HTTP front-end
        // resolves a stack on every request — so trimming on the cache-hit path would walk the pool
        // once per request to discover there is nothing to do. ~keep
        if built {
            self.trim_warm_read_stacks(&entry.key);
        }
        Ok((shared, conn))
    }

    /// Open `entry`'s read side for the daemon to host: a read-only store over the file map the
    /// writer flushed, borrowing the writer's index handle, plus the workspace's config, repository,
    /// git cache and git-history index. Blocking (the store open reads the file map).
    fn host_read_stack(
        self: &std::sync::Arc<Self>,
        entry: &WorkspaceEntry,
        root: &Path,
        git_history: impl FnOnce(&Path) -> Option<crate::comms::git_history_ops::HostedHistory>,
    ) -> anyhow::Result<crate::mcp::HostedReadStack> {
        use anyhow::Context as _;

        let store = self.hosted_store(entry, root).context("open hosted read stack store")?;
        let repo = crate::git::Repo::discover(root).ok().map(std::sync::Arc::new);
        let git_cache = std::sync::Arc::new(
            crate::git_cache::GitCache::open(&store.basemind_dir, crate::mcp::HOSTED_GIT_CACHE_MEM, true)
                .context("open hosted git cache")?,
        );
        let history = if repo.is_some() && crate::git_history::index_enabled() {
            git_history(root)
        } else {
            None
        };
        Ok(crate::mcp::HostedReadStack::start(
            store,
            entry.config(),
            repo,
            git_cache,
            history,
            std::sync::Arc::clone(self),
        ))
    }

    /// A fresh read-only store for a hosted `root` after the writer scanned it: the file map the
    /// scan flushed, borrowing the writer's index handle per call.
    pub(crate) fn read_store(self: &std::sync::Arc<Self>, root: &Path) -> Result<Store, WorkspacePoolError> {
        let entry = self.get_or_open(root)?;
        Ok(self.hosted_store(&entry, root)?)
    }

    /// A read-only store over `entry`'s file map whose index reads borrow the handle of the
    /// workspace's current pool entry, looked up per call (ADR-0042). The store holds no handle, so
    /// a poisoned-handle reopen or an eviction is never kept from releasing the database by a
    /// hosted stack, and a stack that outlives its entry reads the entry that replaced it.
    fn hosted_store(
        self: &std::sync::Arc<Self>,
        entry: &WorkspaceEntry,
        root: &Path,
    ) -> Result<Store, store::StoreError> {
        let mut read = Store::open_read_only_no_index(root, VIEW_WORKING)?;
        let (pool, key) = (std::sync::Arc::downgrade(self), entry.key.clone());
        read.index_source = Some(std::sync::Arc::new(move || {
            let pool = pool.upgrade()?;
            let entry = pool.lock_map().get(&key).cloned()?;
            entry.index()
        }));
        Ok(read)
    }

    /// Drop least-recently-used hosted read stacks until at most [`warm_read_stack_cap`] stay warm,
    /// leaving every workspace's hot store entry in place.
    ///
    /// Follows the store LRU in [`Self::get_or_open`] exactly — same `last_used` clock, same
    /// `active_conns` pin, same "exceed the cap rather than pull the rug from under a live session"
    /// rule — so there is one eviction policy here, expressed twice at two different costs.
    /// `just_built` is never a victim: the caller that paid for the build must be served from it.
    ///
    /// Non-blocking by construction: a slot whose lock is held is one being built or handed out
    /// right now, i.e. the most-recently-used, so skipping it is both cheaper and more correct than
    /// waiting for it.
    fn trim_warm_read_stacks(&self, just_built: &str) {
        use std::sync::atomic::Ordering::Acquire;
        let cap = warm_read_stack_cap();
        let entries: Vec<std::sync::Arc<WorkspaceEntry>> = self.lock_map().values().cloned().collect();
        let mut warm = 0usize;
        let mut evictable: Vec<std::sync::Arc<WorkspaceEntry>> = Vec::new();
        for entry in entries {
            let is_warm = match entry.serve_state.try_lock() {
                Ok(slot) => slot.is_some(),
                Err(_) => continue,
            };
            if !is_warm {
                continue;
            }
            warm += 1;
            if entry.key != just_built && entry.active_conns.load(Acquire) == 0 {
                evictable.push(entry);
            }
        }
        let Some(mut over) = warm.checked_sub(cap).filter(|over| *over > 0) else {
            return;
        };
        evictable.sort_by_key(|entry| entry.last_used());
        for entry in evictable {
            if over == 0 {
                break;
            }
            if let Ok(mut slot) = entry.serve_state.try_lock()
                && slot.take().is_some()
            {
                over -= 1;
                tracing::info!(root = %entry.root.display(), cap, "daemon: shed a warm read stack over the cap");
            }
        }
    }

    /// Drop the hosted read stack of every workspace idle for at least `idle`, KEEPING its hot store
    /// entry, and return the count dropped.
    ///
    /// The counterpart to [`Self::evict_idle`] on a much shorter clock ([`READ_STACK_IDLE_TTL`]): a
    /// read stack is an O(corpus) projection any later connection rebuilds from the same blobs,
    /// whereas the store handle it sits next to holds the workspace's index lock. An entry with a
    /// live connection is skipped, exactly as in the entry sweep — dropping a stack out from under
    /// an in-flight rmcp session would leave that session's `Arc` alive AND let the next connection
    /// build a second stack for the same workspace, which is worse than keeping the first.
    pub(crate) fn evict_idle_read_stacks(&self, idle: Duration) -> usize {
        use std::sync::atomic::Ordering::Acquire;
        let entries: Vec<std::sync::Arc<WorkspaceEntry>> = self.lock_map().values().cloned().collect();
        let mut dropped = 0;
        for entry in entries {
            if entry.active_conns.load(Acquire) != 0 || entry.last_used().elapsed() < idle {
                continue;
            }
            if let Ok(mut slot) = entry.serve_state.try_lock()
                && slot.take().is_some()
            {
                dropped += 1;
            }
        }
        dropped
    }

    /// Drop every hosted read stack, live sessions or not, and return the count dropped. For the
    /// drain: a stack holds this pool and the broker as its hosts, so one left in its slot keeps
    /// both, and every store they hold, alive past the daemon's exit from its serve loop.
    pub(crate) async fn shed_read_stacks(&self) -> usize {
        let entries: Vec<std::sync::Arc<WorkspaceEntry>> = self.lock_map().values().cloned().collect();
        let mut dropped = 0;
        for entry in entries {
            // A slot held this long is mid-build; the runtime teardown cancels that build. ~keep
            let Ok(mut slot) = tokio::time::timeout(Duration::from_secs(1), entry.serve_state.lock()).await else {
                continue;
            };
            if slot.take().is_some() {
                dropped += 1;
            }
        }
        dropped
    }

    /// Number of workspaces currently holding a warm hosted read stack. Exposed for tests: it is the
    /// quantity the cap bounds, and it is deliberately NOT [`Self::len`] — the two diverge on
    /// purpose.
    #[cfg(test)]
    pub(crate) fn warm_read_stacks(&self) -> usize {
        let entries: Vec<std::sync::Arc<WorkspaceEntry>> = self.lock_map().values().cloned().collect();
        entries
            .iter()
            .filter(|entry| entry.serve_state.try_lock().is_ok_and(|slot| slot.is_some()))
            .count()
    }

    /// Register one relay connection against `root`, returning a guard that decrements the live-count
    /// on drop. While any guard is held, eviction (LRU + idle sweep) skips this workspace, so its
    /// shared read stack is never dropped from under an in-flight rmcp session.
    /// A connection served from a stack registers through [`Self::serve_conn`] instead.
    #[cfg(test)]
    pub(crate) fn begin_conn(&self, root: &Path) -> Result<ServeConnGuard, WorkspacePoolError> {
        Ok(Self::register_conn(self.get_or_open(root)?))
    }

    fn register_conn(entry: std::sync::Arc<WorkspaceEntry>) -> ServeConnGuard {
        entry.touch();
        entry.active_conns.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ServeConnGuard { entry }
    }

    /// Async front for [`Self::get_or_open`]: a hot entry is served inline, a cold one is opened on a
    /// blocking thread so the journal replay / seed clone never stalls an async worker (and the
    /// `open_lock` wait never parks one).
    async fn get_or_open_async(
        self: &std::sync::Arc<Self>,
        root: &Path,
    ) -> Result<std::sync::Arc<WorkspaceEntry>, WorkspacePoolError> {
        let key = store::workspace_key(root);
        let hit = self.lock_map().get(&key).cloned();
        if let Some(entry) = hit {
            self.refresh_config(&entry);
            return Ok(entry);
        }
        let pool = std::sync::Arc::clone(self);
        let root = root.to_path_buf();
        let entry = tokio::task::spawn_blocking(move || pool.get_or_open(&root))
            .await
            .map_err(|join| {
                WorkspacePoolError::Store(store::StoreError::Io {
                    path: PathBuf::new(),
                    source: std::io::Error::other(format!("workspace open task failed: {join}")),
                })
            })??;
        self.spawn_pending_rebuild(&entry);
        Ok(entry)
    }

    /// Start the full scan a freshly opened workspace owes when its index was wiped or is empty over
    /// a populated file map, detached on a scan slot: the open must not wait on a whole-tree scan. The
    /// entry's `rebuild_pending` flag makes a racing client rescan run full too, and the scan coalesces.
    /// The flag is read again once the slot is held, so a rebuild queued behind an earlier one that
    /// cleared it does not walk the tree twice.
    fn spawn_pending_rebuild(self: &std::sync::Arc<Self>, entry: &std::sync::Arc<WorkspaceEntry>) {
        use super::scan_slots::RESCAN_SLOTS;

        if !entry.rebuild_pending.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let pool = std::sync::Arc::clone(self);
        let entry = std::sync::Arc::clone(entry);
        let rebuild = move || {
            if !entry.rebuild_pending.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            if let Err(error) = pool.rescan(&entry.root, None, true, false, &pool.scan_cancel) {
                tracing::warn!(root = %entry.root.display(), %error, "index rebuild scan failed");
            }
        };
        tokio::spawn(super::scan_slots::on_blocking_slot(&RESCAN_SLOTS, rebuild));
    }
}
