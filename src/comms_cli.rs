//! CLI helpers for the agent-comms broker lifecycle: the shell statusline snapshot and the
//! `basemind comms` lifecycle subcommands (daemon / start / stop / status). Extracted from
//! `main.rs` to keep the binary root under the module-size cap; behavior is unchanged. Most items
//! are gated on the `comms` feature; `cmd_statusline` compiles unconditionally and is a no-op
//! without it.

#[cfg(all(feature = "comms", any(unix, windows)))]
use anyhow::Context;
use anyhow::Result;

/// Print a statusline. Two modes:
///
/// - `root == Some(path)` (invoked as `basemind statusline --root <path>`): render the compact
///   per-repo line for that workspace, read CHEAPLY from the `status.json` sidecar + `telemetry.jsonl`
///   — never opening the Fjall index (no [`basemind::store::Store::open`], no index recovery), so it
///   is safe to refresh every few seconds. This is the path the shell plugin delegates to when the
///   index lives in the machine-global cache (nothing in the repo to read).
/// - `root == None` (invoked as bare `basemind statusline`): the daemon hot-workspace summary
///   (unchanged). Fast and silent: a missing daemon prints nothing and exits 0. Without the `comms`
///   feature there is no daemon, so that path is a no-op.
pub(crate) fn cmd_statusline(root: Option<&std::path::Path>) -> Result<()> {
    if let Some(root) = root {
        println!("{}", render_repo_statusline(root));
        return Ok(());
    }
    #[cfg(all(feature = "comms", any(unix, windows)))]
    {
        use basemind::comms::client::CommsClient;
        use basemind::comms::ids::AgentId;
        use basemind::comms::singleton;

        let line = (|| -> Option<String> {
            let paths = singleton::resolve_paths().ok()?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(async move {
                let agent = AgentId::parse("basemind-statusline").ok()?;
                let mut client = CommsClient::connect(&paths, agent, None, None).await.ok()?;
                let hot = client.accessed_paths().await.ok()?;
                Some(format_statusline(&hot))
            })
        })();
        if let Some(line) = line {
            println!("{line}");
        }
    }
    Ok(())
}

// ANSI palette mirroring `.claude-plugin/statusline.sh` so the delegated line matches the shell's
// aesthetic. True-color brand orange (#F97316) + 256-color accents; a single `\x1b[0m` resets each span.
const BRAND: &str = "\x1b[38;2;249;115;22m";
const CYAN: &str = "\x1b[38;5;51m";
const MAGENTA: &str = "\x1b[38;5;201m";
const LABEL: &str = "\x1b[38;5;255m";
const SEP: &str = "\x1b[38;5;240m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const BRAND_GLYPH: &str = "◆";

/// The `◆ basemind` brand mark, matching the shell renderer's `mark()`.
fn brand_mark() -> String {
    format!("{BRAND}{BRAND_GLYPH}{RESET} {BOLD}{BRAND}basemind{RESET}")
}

/// Render the compact per-repo statusline for `root`, reading ONLY the cheap `status.json` sidecar
/// and `telemetry.jsonl` tail — never opening the index. When the workspace has no sidecar (never
/// scanned, or an unrecognized schema), returns the same "no index" hint the shell shows so the bar
/// is never blank.
fn render_repo_statusline(root: &std::path::Path) -> String {
    use basemind::store::{read_status_sidecar, workspace_cache_dir};

    let basemind_dir = workspace_cache_dir(root);
    let Some(status) = read_status_sidecar(&basemind_dir) else {
        return format!(
            "{} {SEP}│{RESET} {LABEL}no index — run:{RESET} {BOLD}{CYAN}basemind scan{RESET}",
            brand_mark()
        );
    };

    let age = format_scan_age(status.scanned_unix);
    let (calls, saved) = telemetry_today(&basemind_dir);

    let mut out = format!(
        "{}  {BOLD}{CYAN}{}{RESET} {LABEL}files{RESET} {SEP}·{RESET} {BOLD}{CYAN}{age}{RESET}",
        brand_mark(),
        fmt_count(status.file_count as u64),
    );
    out.push_str(&format!(
        "  {SEP}│{RESET}  {BOLD}{MAGENTA}{}{RESET} {LABEL}calls{RESET} {SEP}·{RESET} {BOLD}{MAGENTA}{}{RESET} {LABEL}saved{RESET}",
        fmt_count(calls),
        fmt_count(saved),
    ));
    out
}

/// Human-readable age of a Unix-epoch-seconds scan timestamp (`Ns/Nm/Nh/Nd ago`), mirroring the
/// shell renderer's buckets. `"never"` when the timestamp is non-positive or in the future.
fn format_scan_age(scanned_unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now - scanned_unix;
    if scanned_unix <= 0 || delta < 0 {
        return "never".to_string();
    }
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3_600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3_600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

/// One telemetry row, read for its two aggregate fields only. Unknown fields are ignored by serde,
/// so this stays forward-compatible with the full `TelemetryRow` schema without coupling to it.
#[derive(serde::Deserialize)]
struct StatuslineTelemetryRow {
    ts_micros: i64,
    #[serde(default)]
    est_tokens_saved: u64,
}

/// Aggregate today's `(calls, est_tokens_saved)` from `telemetry.jsonl`, tailing the last rows and
/// counting those within the last 24h — the same "today" window the MCP telemetry summary uses.
/// Best-effort: a missing/unreadable log yields `(0, 0)`.
fn telemetry_today(basemind_dir: &std::path::Path) -> (u64, u64) {
    use std::io::{BufRead, BufReader};

    const TAIL_ROWS: usize = 2_000;
    const DAY_MICROS: i64 = 24 * 3_600 * 1_000_000;

    let now_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let cutoff = now_micros.saturating_sub(DAY_MICROS);

    let Ok(file) = std::fs::File::open(basemind_dir.join("telemetry.jsonl")) else {
        return (0, 0);
    };
    let mut tail: std::collections::VecDeque<StatuslineTelemetryRow> =
        std::collections::VecDeque::with_capacity(TAIL_ROWS);
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(row) = serde_json::from_str::<StatuslineTelemetryRow>(&line) {
            if tail.len() == TAIL_ROWS {
                tail.pop_front();
            }
            tail.push_back(row);
        }
    }
    let mut calls = 0u64;
    let mut saved = 0u64;
    for row in tail.iter().filter(|r| r.ts_micros >= cutoff) {
        calls += 1;
        saved = saved.saturating_add(row.est_tokens_saved);
    }
    (calls, saved)
}

/// Compact count formatting mirroring the shell renderer's `fmt_count`: plain under 1k, one-decimal
/// `k` under 10k, integer `k` under 1M, integer `M` beyond.
fn fmt_count(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{}.{}k", n / 1_000, (n * 10 / 1_000) % 10)
    } else if n < 1_000_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{}M", n / 1_000_000)
    }
}

/// Render the daemon's hot-workspace snapshot into one compact line (e.g. `bm: web · api +2 · 5
/// hot`). An empty set — daemon up but nothing hot — reads `bm: idle`. Names are the workspace
/// directory basenames; the list is capped so the line stays short regardless of the hot count.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn format_statusline(workspaces: &[basemind::comms::workspace_pool::AccessedWorkspace]) -> String {
    if workspaces.is_empty() {
        return "bm: idle".to_string();
    }
    const MAX_NAMES: usize = 3;
    let names: Vec<&str> = workspaces
        .iter()
        .take(MAX_NAMES)
        .map(|w| w.root.file_name().and_then(|n| n.to_str()).unwrap_or("?"))
        .collect();
    let mut label = names.join(" · ");
    if workspaces.len() > MAX_NAMES {
        label.push_str(&format!(" +{}", workspaces.len() - MAX_NAMES));
    }
    format!("bm: {label} · {} hot", workspaces.len())
}

/// Dispatch a comms lifecycle subcommand. Each command drives a small current-thread tokio
/// runtime — the broker daemon itself uses a multi-thread runtime so concurrent links don't
/// serialize.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) fn cmd_comms(action: crate::CommsLifecycleCmd, json: bool) -> Result<()> {
    match action {
        crate::CommsLifecycleCmd::Daemon => basemind::cli::comms_daemon::run(),
        crate::CommsLifecycleCmd::Start => cmd_comms_start(),
        crate::CommsLifecycleCmd::Stop { all: true } => cmd_comms_stop_all(json),
        crate::CommsLifecycleCmd::Stop { all: false } => cmd_comms_lifecycle_rpc(CommsRpc::Stop, json),
        crate::CommsLifecycleCmd::Status => cmd_comms_lifecycle_rpc(CommsRpc::Status, json),
        crate::CommsLifecycleCmd::Doctor { probe, clear_fatal } => cmd_comms_doctor(json, probe, clear_fatal),
    }
}

#[cfg(all(feature = "comms", any(unix, windows)))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommsRpc {
    Stop,
    Status,
}

/// How long `daemon ensure` waits for the streamable-HTTP transport to answer after ensuring the
/// daemon is up. Generous relative to a cold daemon spawn + bind.
#[cfg(all(feature = "comms", any(unix, windows)))]
const HTTP_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Dispatch a `basemind daemon` subcommand.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(crate) fn cmd_daemon(action: crate::DaemonCmd, json: bool) -> Result<()> {
    match action {
        crate::DaemonCmd::Ensure => cmd_daemon_ensure(json),
    }
}

/// Ensure the daemon is running and its streamable-HTTP MCP transport is ready, then print the base
/// URL. This is what a launcher/hook calls; it only implements the verb (no manifest wiring here).
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_daemon_ensure(json: bool) -> Result<()> {
    use basemind::comms::http_frontend;
    use basemind::comms::singleton;

    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let addr = runtime.block_on(async move {
        singleton::ensure_daemon(&paths)
            .await
            .map_err(|e| anyhow::anyhow!("ensure comms daemon: {e}"))?;
        http_frontend::await_http_ready(&paths.comms_dir, HTTP_READY_TIMEOUT)
            .await
            .context("wait for streamable-HTTP MCP transport")
    })?;

    let url = http_frontend::base_url(&addr);
    if json {
        println!("{{\"ready\":true,\"addr\":\"{addr}\",\"url\":\"{url}\"}}");
    } else {
        println!("{url}");
    }
    Ok(())
}

/// Ensure a daemon is running, spawning it detached if needed.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_start() -> Result<()> {
    use basemind::comms::singleton;
    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let socket_path = paths.socket_path.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        singleton::ensure_daemon(&paths)
            .await
            .map_err(|e| anyhow::anyhow!("ensure comms daemon: {e}"))
    })?;
    println!("comms daemon is running ({})", socket_path.display());
    Ok(())
}

/// Connect to the running daemon and issue a Stop or Status RPC.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_lifecycle_rpc(rpc: CommsRpc, json: bool) -> Result<()> {
    use basemind::comms::client::CommsClient;
    use basemind::comms::singleton;

    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let comms_dir = paths.comms_dir.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let verdict = runtime.block_on(async move {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let agent = basemind::comms::identity::cli_agent_id(&root);
        match rpc {
            CommsRpc::Stop => {
                // Bound the whole RPC, and never propagate its failure. A daemon that cannot answer
                // is precisely the one an operator is trying to stop, and it fails in two ways that
                // both used to defeat `stop`: a poisoned store fails the Hello handshake, so the
                // command aborted before doing anything; and a daemon wedged before its accept loop
                // never completes `connect` at all, so the command hung indefinitely. Time-box it
                // and report what the RPC managed — the caller escalates to the pid. ~keep
                match tokio::time::timeout(STOP_RPC_TIMEOUT, async {
                    CommsClient::connect(&paths, agent, None, None).await?.stop().await
                })
                .await
                {
                    Ok(Ok(())) => return Ok::<StopVerdict, anyhow::Error>(StopVerdict::Accepted),
                    // A daemon that answered with a refusal made an informed decision — `daemon_busy`
                    // means persistent clients are still attached — so surface it and stop here. The
                    // one exception is `store_error`: that daemon answered only to say it cannot
                    // serve, which is the wedge the pid escalation exists for. ~keep
                    Ok(Err(error)) => {
                        if !is_broken_store_error(&error) {
                            return Err(anyhow::anyhow!("stop: {error}"));
                        }
                        tracing::warn!(%error, "comms: daemon reports a broken store; falling back to pid reclaim");
                        return Ok(StopVerdict::Reclaimable);
                    }
                    Err(_) => {
                        tracing::warn!(
                            timeout_secs = STOP_RPC_TIMEOUT.as_secs(),
                            "comms: daemon did not answer the stop RPC in time; falling back to pid reclaim"
                        );
                        return Ok(StopVerdict::Reclaimable);
                    }
                }
            }
            CommsRpc::Status => {
                let mut client = CommsClient::connect(&paths, agent, None, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("connect to comms daemon: {e}"))?;
                let status = client.status().await.map_err(|e| anyhow::anyhow!("status: {e}"))?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&status).map_err(|e| anyhow::anyhow!("serialize status: {e}"))?
                    );
                } else {
                    println!(
                        "pid={} version={} build={} proto={} uptime={}s threads={} subscribers={}",
                        status.pid,
                        status.version,
                        if status.build_id.is_empty() {
                            "unreported"
                        } else {
                            &status.build_id
                        },
                        status.proto_ver,
                        status.uptime_secs,
                        status.threads,
                        status.subscribers,
                    );
                    // Same version, different binary: the case every version check passes and
                    // nobody thinks to look for. Say it outright — the symptom is a daemon quietly
                    // answering with the code it was built from, not the code just installed.
                    let ours = basemind::version::build_id();
                    if !status.build_id.is_empty() && status.build_id != ours {
                        println!(
                            "  WARNING: this daemon is running a DIFFERENT build of the same version \
                             (daemon {} vs this binary {}).",
                            status.build_id, ours
                        );
                        println!(
                            "  It will keep answering with its own code — a version check cannot see this. \
                             Restart it with `basemind comms stop` to pick up the current binary."
                        );
                    }
                }
            }
        }
        Ok::<StopVerdict, anyhow::Error>(StopVerdict::Accepted)
    })?;

    if !matches!(rpc, CommsRpc::Stop) {
        return Ok(());
    }
    // Confirm the daemon for THIS comms dir actually went away, escalating to its pid if it did not.
    // `asked_to_stop` only records whether the RPC was accepted — a daemon can accept a Stop and
    // still be wedged before it reaches the drain, so the pid is the thing worth verifying. ~keep
    let ours: Vec<_> = basemind::daemon_lock::live_daemons_of(basemind::daemon_lock::DaemonKind::Comms)
        .into_iter()
        .filter(|record| record.dir == comms_dir)
        .collect();
    let forced = reclaim_unresponsive(&ours);
    let accepted = verdict == StopVerdict::Accepted;
    if json {
        println!(
            "{}",
            serde_json::json!({ "stopped": accepted, "force_terminated": forced })
        );
    } else if forced > 0 {
        println!("comms daemon did not answer; terminated it by pid");
    } else if accepted {
        println!("comms daemon stopping");
    } else {
        println!("no live comms daemon for {}", comms_dir.display());
    }
    Ok(())
}

/// Unix seconds now, or `0` if the clock is before the epoch.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `basemind comms doctor`: enumerate the live daemons registered on this machine — every family in
/// the shared registry, each row tagged with its `kind` — and flag a pile-up over the ceiling.
///
/// The default report is pure and cheap — it reads the pidfile registry (pruning dead holders) and
/// issues no daemon RPC — so it is safe to run even when the machine is in a bad state. That
/// property is worth keeping: a diagnostic that hangs when the daemon hangs is useless. But it means
/// "live" here is a *process-liveness* claim, not a health claim: a daemon whose store has become
/// unusable still holds its pid, its flock and its socket, and shows up in this list looking exactly
/// like a healthy one. So every report says which question it answered, and `--probe` asks the other
/// one — a bounded `Status` RPC per comms daemon, reported as a separate `serving` verdict.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_doctor(json: bool, probe: bool, clear_fatal: bool) -> Result<()> {
    use basemind::comms::{singleton, store_health};
    use basemind::daemon_lock::{self, DaemonKind};

    if clear_fatal {
        let paths = singleton::resolve_paths().context("resolve comms paths")?;
        let cleared = store_health::clear(&paths.comms_dir);
        if json {
            println!("{}", serde_json::json!({ "cleared_fatal": cleared }));
        } else if cleared {
            println!(
                "cleared the recorded fatal store error for {}",
                paths.comms_dir.display()
            );
        } else {
            println!("no recorded fatal store error for {}", paths.comms_dir.display());
        }
        return Ok(());
    }

    let daemons = daemon_lock::live_daemons();
    let ceiling = daemon_lock::max_live_daemons();
    let now = now_unix();

    // Probe only the comms family: `agent` and `shells` daemons are registered in the same registry
    // but do not speak this protocol on a comms socket, so a Status RPC there would be meaningless.
    let verdicts: Vec<Option<singleton::DaemonProbe>> = daemons
        .iter()
        .map(|record| {
            (probe && record.kind == DaemonKind::Comms)
                .then(|| singleton::probe_serving(&singleton::comms_socket_path(&record.dir)))
        })
        .collect();

    // Scoped to THIS comms dir, not every registered daemon's: a fatal record means the daemon that
    // wrote it has already exited, so it never has a row above to hang off. The dir is named in the
    // output so an operator with several comms dirs knows which one the corpse belongs to.
    let fatal = singleton::resolve_paths()
        .ok()
        .and_then(|paths| store_health::read(&paths.comms_dir).map(|record| (paths.comms_dir, record)));

    if json {
        let items: Vec<serde_json::Value> = daemons
            .iter()
            .zip(&verdicts)
            .map(|(record, verdict)| {
                let mut row = serde_json::json!({
                    "pid": record.pid,
                    "kind": record.kind,
                    "dir": record.dir,
                    "version": record.version,
                    "uptime_secs": (now - record.started_unix).max(0),
                });
                if let Some(verdict) = verdict {
                    row["reachable"] = probe_json(verdict);
                }
                row
            })
            .collect();
        let report = serde_json::json!({
            "count": daemons.len(),
            "ceiling": ceiling,
            "over_ceiling": daemons.len() > ceiling,
            "checked": if probe { "registry+probe" } else { "registry" },
            "daemons": items,
            "last_fatal_store_error": fatal.as_ref().map(|(dir, record)| serde_json::json!({
                "comms_dir": dir,
                "record": record,
            })),
        });
        println!("{report}");
        return Ok(());
    }

    if daemons.is_empty() {
        println!("no live basemind daemons");
    } else {
        println!("{} live daemon(s) (ceiling {ceiling}):", daemons.len());
        for (record, verdict) in daemons.iter().zip(&verdicts) {
            println!(
                "  pid={} kind={} version={} uptime={}s{} dir={}",
                record.pid,
                record.kind,
                record.version,
                (now - record.started_unix).max(0),
                verdict.as_ref().map(probe_label).unwrap_or_default(),
                record.dir.display(),
            );
        }
        if daemons.len() > ceiling {
            println!(
                "WARNING: {} daemons exceed the ceiling of {ceiling}; run `basemind comms stop --all` to reclaim",
                daemons.len(),
            );
        }
    }

    if let Some((dir, record)) = &fatal {
        println!(
            "\nlast fatal store error in {} (pid {}, {} ago, on `{}`, build {}):\n  {}",
            dir.display(),
            record.pid,
            humanize_age(now.saturating_sub(record.epoch_secs as i64)),
            record.request,
            record.version,
            record.error,
        );
        println!("  That daemon released its lock and exited. Acknowledge with `basemind comms doctor --clear-fatal`.");
    }

    if probe {
        println!("\nchecked: the pidfile registry, plus a Status RPC per comms daemon (`serving` above).");
    } else {
        println!("\nchecked: the pidfile registry only — whether a process with that pid exists.");
        println!(
            "  This does NOT mean a daemon is serving. Use `basemind comms doctor --probe`, or `basemind comms status`, to test that."
        );
    }
    Ok(())
}

/// The trailing serviceability verdict on a `doctor --probe` row. Empty when the row was not probed,
/// so an unprobed row renders exactly as it did before.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn probe_label(verdict: &basemind::comms::singleton::DaemonProbe) -> String {
    use basemind::comms::singleton::DaemonProbe;
    match verdict {
        DaemonProbe::Serving { threads } => format!(" serving({threads} threads)"),
        DaemonProbe::Failing { code, message } => format!(" LIVE BUT FAILING [{code}: {message}]"),
        DaemonProbe::Unreachable => " LIVE BUT NOT RESPONDING".to_string(),
    }
}

#[cfg(all(feature = "comms", any(unix, windows)))]
fn probe_json(verdict: &basemind::comms::singleton::DaemonProbe) -> serde_json::Value {
    use basemind::comms::singleton::DaemonProbe;
    match verdict {
        DaemonProbe::Serving { threads } => serde_json::json!({ "state": "serving", "threads": threads }),
        DaemonProbe::Failing { code, message } => {
            serde_json::json!({ "state": "failing", "code": code, "message": message })
        }
        DaemonProbe::Unreachable => serde_json::json!({ "state": "not_responding" }),
    }
}

/// Coarse "N ago" for operator output. Whole units only — the exact epoch is in the JSON report.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn humanize_age(secs: i64) -> String {
    match secs {
        s if s < 0 => "an unknown time".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// `basemind comms stop --all`: signal every live comms daemon on this machine to drain, addressing
/// each by its own socket. Uses the low-level [`singleton::request_stop`] rather than a `CommsClient`
/// (which could respawn the very daemon it meant to stop).
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_stop_all(json: bool) -> Result<()> {
    use basemind::comms::singleton;
    use basemind::daemon_lock::{self, DaemonKind};

    // Comms-only: this addresses each holder over the comms stop protocol, which another daemon
    // family in the shared registry does not speak.
    let daemons = daemon_lock::live_daemons_of(DaemonKind::Comms);
    // Classify each answer rather than discarding it. A daemon that ignored the request is exactly
    // the case `stop` most needs to handle — asking it to drain is itself a request, and a daemon
    // whose store is broken can refuse it, so the documented recovery path would otherwise route
    // through the very subsystem that is down. But a daemon that answered `daemon_busy` made an
    // informed decision to protect the persistent clients attached to it, and reclaiming THAT one
    // by pid is the severing the refusal exists to prevent. Only the former may be escalated. ~keep
    let mut reclaimable = Vec::new();
    let mut refused = Vec::new();
    for record in &daemons {
        let outcome = singleton::request_stop_classified(&singleton::comms_socket_path(&record.dir));
        if outcome.permits_reclaim() {
            reclaimable.push(record.clone());
        } else if let singleton::StopOutcome::Refused { code, message } = outcome {
            refused.push((record.pid, code, message));
        }
    }
    let forced = reclaim_unresponsive(&reclaimable);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "stopped": reclaimable.len(),
                "force_terminated": forced,
                "refused": refused
                    .iter()
                    .map(|(pid, code, message)| serde_json::json!({
                        "pid": pid,
                        "code": code,
                        "message": message,
                    }))
                    .collect::<Vec<_>>(),
            })
        );
    } else if daemons.is_empty() {
        println!("no live basemind daemons to stop");
    } else {
        println!("asked {} daemon(s) to stop", daemons.len());
        if forced > 0 {
            println!("  force-terminated {forced} daemon(s) that did not answer the stop request");
        }
        for (pid, code, message) in &refused {
            println!("  pid={pid} refused [{code}]: {message}");
            println!("    left running — stopping it would disconnect the clients it is serving");
        }
    }
    Ok(())
}

/// What the `Stop` RPC established, and therefore whether escalating to the pid is warranted.
#[cfg(all(feature = "comms", any(unix, windows)))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum StopVerdict {
    /// The daemon took the request (or there was nothing to ask).
    Accepted,
    /// Nothing answered, or the answer proved the store is broken.
    Reclaimable,
}

/// Whether a failed `Stop` came back as the broker's `store_error` — the one refusal that is not a
/// decision but a symptom, and so does not protect the daemon from being reclaimed.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn is_broken_store_error(error: &basemind::comms::client::CommsClientError) -> bool {
    matches!(error, basemind::comms::client::CommsClientError::Broker { code, .. } if code == "store_error")
}

/// Hard bound on the `Stop` RPC itself — connect plus request. A daemon wedged before its accept
/// loop never completes the connect, which used to hang `basemind comms stop` indefinitely: the
/// documented recovery path blocked on the very daemon it was meant to reclaim.
#[cfg(all(feature = "comms", any(unix, windows)))]
const STOP_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a daemon has to honour a `Stop` request before [`reclaim_unresponsive`] escalates to
/// its pid. Generous relative to a drain that is actually progressing (the daemon's own drain grace
/// is 10s), so a busy-but-healthy daemon is never killed out from under an in-flight request.
#[cfg(all(feature = "comms", any(unix, windows)))]
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(12);

/// Wait out [`STOP_GRACE`] and terminate any daemon still alive, returning how many were forced.
///
/// This is the half of recovery that `stop` was missing. `request_stop` is best-effort by design,
/// so a daemon that cannot serve simply stays put — holding the singleton flock and the socket,
/// with `pid_is_live` keeping its registry row alive, indefinitely.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn reclaim_unresponsive(daemons: &[basemind::daemon_lock::DaemonRecord]) -> usize {
    use basemind::comms::singleton;
    use basemind::daemon_lock::pid_is_live;

    let deadline = std::time::Instant::now() + STOP_GRACE;
    let mut stubborn: Vec<u32> = daemons.iter().map(|record| record.pid).collect();
    while std::time::Instant::now() < deadline {
        stubborn.retain(|pid| pid_is_live(*pid));
        if stubborn.is_empty() {
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    stubborn
        .iter()
        .filter(|pid| {
            tracing::warn!(pid, "comms: daemon ignored the stop request; terminating by pid");
            singleton::force_terminate(**pid, std::time::Duration::from_secs(3))
        })
        .count()
}

#[cfg(all(test, feature = "comms", any(unix, windows)))]
mod statusline_tests {
    use std::path::PathBuf;

    use basemind::comms::workspace_pool::AccessedWorkspace;

    fn ws(root: &str) -> AccessedWorkspace {
        AccessedWorkspace {
            root: PathBuf::from(root),
            key: "k".to_string(),
            idle_secs: 0,
        }
    }

    #[test]
    fn empty_hot_set_reads_idle() {
        assert_eq!(super::format_statusline(&[]), "bm: idle");
    }

    #[test]
    fn lists_workspace_basenames_and_the_hot_count() {
        let hot = [ws("/repos/web"), ws("/repos/api")];
        assert_eq!(super::format_statusline(&hot), "bm: web · api · 2 hot");
    }

    #[test]
    fn caps_the_name_list_with_an_overflow_marker() {
        let hot = [ws("/a/one"), ws("/a/two"), ws("/a/three"), ws("/a/four"), ws("/a/five")];
        assert_eq!(super::format_statusline(&hot), "bm: one · two · three +2 · 5 hot");
    }
}
