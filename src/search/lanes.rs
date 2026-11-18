//! Why the fjall-backed `code_search` lanes (keyword and exact) did not run in a session, and what
//! the caller can do about it. The MCP adapter turns a [`LaneCause`] into its error.

/// The two fjall-backed lanes' rankings, plus why they could not run.
pub(crate) struct LaneOutcome {
    /// BM25 `(chunk_id, score)`, best first.
    pub(crate) keyword: Vec<(String, f32)>,
    /// Exact symbol-name lane chunk ids, best first.
    pub(crate) exact: Vec<String>,
    /// `None` when the lanes ran. `Some` when they could not — never conflated with "ran and
    /// matched nothing".
    pub(crate) degraded: Option<Degraded>,
}

impl From<Degraded> for LaneOutcome {
    fn from(degraded: Degraded) -> Self {
        Self {
            keyword: Vec::new(),
            exact: Vec::new(),
            degraded: Some(degraded),
        }
    }
}

/// What the caller can do about lanes that did not run, which picks the error a call that has no
/// other lane answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaneCause {
    /// Another process holds the workspace's index and nothing serves it to this session: retry,
    /// as for every lock contention (the CLI exits `3`).
    Held,
    /// The index must be rebuilt before it can be read.
    RescanOwed,
    /// The daemon's read for a hosted session failed. A session's own read cannot fail, so without a
    /// daemon to host sessions nothing constructs it. The variant stays (rather than a `cfg` on it)
    /// because its match arm in the MCP adapter would grow `src/mcp` past the layering ratchet.
    #[cfg_attr(
        not(all(feature = "comms", any(unix, windows))),
        expect(dead_code, reason = "only a daemon-hosted session's read can fail")
    )]
    Failed,
}

/// Why the lanes did not run, and the reason text `lanes_unavailable` carries.
#[derive(Debug)]
pub(crate) struct Degraded {
    pub(crate) reason: String,
    pub(crate) cause: LaneCause,
}

impl Degraded {
    pub(crate) fn new(cause: LaneCause, reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            cause,
        }
    }

    /// A session that is not hosted by the daemon and holds no index view: the scan owes a
    /// rebuild, or another process holds the index (a writer `serve`, or a scan).
    pub(crate) fn unreachable(rescan_owed: bool) -> Self {
        if rescan_owed {
            return Self::new(
                LaneCause::RescanOwed,
                "the index of this workspace must be rebuilt: run `basemind scan` (MCP: `index_rescan`)",
            );
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        let fix = "start the daemon (`basemind comms start`), which then serves the index to every session, or \
                   stop the process that holds the lock, then retry";
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        let fix = "stop the process that holds the lock, then retry";
        Self::new(
            LaneCause::Held,
            format!("another basemind process holds this workspace's index and no daemon serves it: {fix}"),
        )
    }
}
