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
        use basemind::comms::client::{ClientHello, CommsClient};
        use basemind::comms::singleton;

        let line = (|| -> Option<String> {
            let paths = singleton::resolve_paths().ok()?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(async move {
                let mut client = CommsClient::connect(&paths, ClientHello::anonymous()).await.ok()?;
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
    if status.is_degraded() {
        out.push_str(&format!("  {SEP}│{RESET}  {BOLD}{MAGENTA}degraded{RESET}"));
    }
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
        crate::CommsLifecycleCmd::Stop { all: true } => crate::comms_stop::cmd_comms_stop_all(json),
        crate::CommsLifecycleCmd::Stop { all: false } => crate::comms_stop::cmd_comms_stop(json),
        crate::CommsLifecycleCmd::Status => cmd_comms_status(json),
        crate::CommsLifecycleCmd::Doctor { probe, clear_fatal } => cmd_comms_doctor(json, probe, clear_fatal),
        crate::CommsLifecycleCmd::Cleanup { dry_run, older_than } => cmd_comms_cleanup(json, dry_run, older_than),
    }
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

    let comms_dir = paths.comms_dir.clone();
    let addr = runtime.block_on(async move {
        // The HTTP grant in this environment is an operator asking for the transport, so it is
        // passed to a daemon this call starts; without it this is an implicit caller like any hook.
        let ensured = if http_frontend::http_enabled() {
            singleton::ensure_daemon_explicit(&paths).await
        } else {
            singleton::ensure_daemon(&paths).await
        };
        ensured.map_err(|e| anyhow::anyhow!("ensure comms daemon: {e}"))?;
        // The daemon is up either way; only the HTTP front-end is conditional. It is opt-in, so
        // "no transport" is the ordinary outcome and must not read as a failure to callers (the
        // session hooks call this on every start) — hence `Ok(None)` rather than an error.
        if !http_frontend::http_enabled() {
            return anyhow::Ok(None);
        }
        http_frontend::await_http_ready(&paths.comms_dir, HTTP_READY_TIMEOUT)
            .await
            .context("wait for streamable-HTTP MCP transport")
            .map(Some)
    })?;

    let Some(addr) = addr else {
        let grant = http_frontend::ALLOW_HTTP_ENV;
        if json {
            println!("{{\"ready\":false,\"http\":\"disabled\",\"grant\":\"{grant}\"}}");
        } else {
            println!(
                "comms daemon is running; its streamable-HTTP MCP transport is opt-in and not \
                 enabled (set {grant}=1 in the daemon's environment and restart it)"
            );
        }
        return Ok(());
    };

    let url = http_frontend::base_url(&addr);
    // The transport now requires a bearer token, so an address alone is no longer actionable.
    let token = http_frontend::published_token(&comms_dir).unwrap_or_default();
    if json {
        println!("{{\"ready\":true,\"addr\":\"{addr}\",\"url\":\"{url}\",\"token\":\"{token}\"}}");
    } else {
        // The URL stays alone on stdout so `URL=$(basemind daemon ensure)` keeps working; the
        // credential goes to stderr, where it informs a human without joining a captured value. ~keep
        println!("{url}");
        if token.is_empty() {
            eprintln!(
                "warning: the daemon published no bearer token, so this URL will be refused. It \
                 predates the token requirement — restart it to mint one."
            );
        } else {
            eprintln!("Authorization: Bearer {token}");
        }
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
        // Explicit, so `BASEMIND_NO_AUTOSPAWN` does not apply: an operator who typed `comms start`
        // has stated the intent the variable exists to withhold from implicit callers.
        singleton::ensure_daemon_explicit(&paths)
            .await
            .map_err(|e| anyhow::anyhow!("ensure comms daemon: {e}"))
    })?;
    println!("comms daemon is running ({})", socket_path.display());
    Ok(())
}

/// `basemind comms cleanup`: remove the rendezvous of other protocol generations, and of releases
/// up to 0.33, that no daemon holds and nothing has used for `older_than_days`. Reads locks and
/// file ages only; this generation's directory is never a candidate. With `older_than_days` zero
/// the age test is off, so the previous release's message store goes as soon as none of its
/// daemons is running. It also removes the registry records other generations left for daemons
/// that are gone, which otherwise count toward the daemon ceiling whenever their pid is reused.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_cleanup(json: bool, dry_run: bool, older_than_days: u64) -> Result<()> {
    use basemind::comms::generation_cleanup::{self, Rendezvous, Verdict};
    const DAY_SECS: u64 = 24 * 60 * 60;

    let base = basemind::comms::singleton::comms_base_dir().context("resolve the comms directory")?;
    let max_age = std::time::Duration::from_secs(older_than_days.saturating_mul(DAY_SECS));
    let findings = generation_cleanup::sweep(&base, max_age, dry_run, std::time::SystemTime::now());
    // After the sweep, so that the record of a daemon whose directory it just removed goes too.
    let stale_records = basemind::daemon_lock::prune_other_generations_records(dry_run);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "dry_run": dry_run,
                "rendezvous": findings,
                "stale_registry_records": stale_records,
            })
        );
    } else if findings.is_empty() {
        println!("nothing to clean up in {}", base.display());
    }
    if !json && stale_records > 0 {
        let outcome = if dry_run { "would remove" } else { "removed" };
        println!("{outcome}: {stale_records} registry record(s) of other generations whose daemon is gone");
    }
    for finding in findings.iter().filter(|_| !json) {
        let name = match &finding.rendezvous {
            Rendezvous::Generation(name) => base.join(name).display().to_string(),
            Rendezvous::PreGeneration => format!("{} (files of releases up to 0.33)", base.display()),
            Rendezvous::Leftover(name) => format!("{} (left by an interrupted cleanup)", base.join(name).display()),
        };
        let outcome = match finding.verdict {
            Verdict::Removed if dry_run => "would remove",
            Verdict::Removed => "removed",
            Verdict::InUse => "kept (a daemon holds its lock)",
            Verdict::Recent => "kept (used too recently)",
            Verdict::Skipped => "skipped (no daemon.lock, so not a rendezvous)",
            Verdict::Failed => "could not remove all of",
        };
        println!(
            "{outcome}: {name} — {} bytes, idle {} days",
            finding.bytes,
            finding.idle_secs / DAY_SECS
        );
    }
    anyhow::ensure!(
        findings.iter().all(|finding| finding.verdict != Verdict::Failed),
        "some files could not be removed"
    );
    Ok(())
}

/// `basemind comms status`: connect to the running daemon and print what it reports.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_status(json: bool) -> Result<()> {
    use basemind::comms::client::{ClientHello, CommsClient};
    use basemind::comms::singleton;

    let paths = singleton::resolve_paths().context("resolve comms paths")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        let mut client = CommsClient::connect(&paths, ClientHello::anonymous())
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
                "pid={} version={} build={} proto={} uptime={}s agent_store={}",
                status.pid,
                status.version,
                if status.build_id.is_empty() {
                    "unreported"
                } else {
                    &status.build_id
                },
                status.proto_ver,
                status.uptime_secs,
                status.agent_store,
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
        Ok(())
    })
}

/// Unix seconds now, or `0` if the clock is before the epoch.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `basemind comms doctor`: enumerate this protocol generation's live daemons registered on this
/// machine — every family in the registry, each row tagged with its `kind` — and flag a pile-up
/// over the ceiling. Daemons of other generations (a release up to 0.33, say) are counted from
/// their registry records and never listed or probed: they may not speak this binary's protocol.
///
/// The default report is pure and cheap — it reads the pidfile registry (pruning dead holders) and
/// issues no daemon RPC — so it is safe to run even when the machine is in a bad state. That
/// property is worth keeping: a diagnostic that hangs when the daemon hangs is useless. But it means
/// "live" here is a *process-liveness* claim, not a health claim: a daemon whose store has become
/// unusable still holds its pid, its flock and its socket, and shows up in this list looking exactly
/// like a healthy one. So every report says which question it answered, and `--probe` asks the other
/// one — a bounded `Status` RPC per comms daemon, reported as a separate `serving` verdict.
/// `--clear-fatal`: acknowledge the records of scans that died without unwinding. Only records
/// whose pid is dead are removed, so a scan running right now keeps its claim.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn clear_recorded_failures(json: bool) -> Result<()> {
    let cleared_scans = basemind::scan_evidence::stale_inflight_scans()
        .iter()
        .filter(|scan| basemind::scan_evidence::clear(&scan.cache_dir))
        .count();
    if json {
        println!("{}", serde_json::json!({ "cleared_stale_scans": cleared_scans }));
        return Ok(());
    }
    println!("cleared {cleared_scans} stale scan-inflight record(s)");
    Ok(())
}

/// Report scans whose breadcrumb outlived their process.
///
/// The distinction the record exists to draw: a scan that fails normally unwinds and removes its
/// own record, so a surviving one proves no destructor ran — a kill signal, and on a scan
/// overwhelmingly the kernel's OOM killer.
#[cfg(all(feature = "comms", any(unix, windows)))]
fn report_stale_scans(stale_scans: &[basemind::scan_evidence::StaleScan], now: i64) {
    if stale_scans.is_empty() {
        return;
    }
    println!(
        "\n{} scan(s) left a breadcrumb behind and their process is gone:",
        stale_scans.len()
    );
    for scan in stale_scans {
        println!(
            "  pid={} phase={} candidates={} started {} ago build={} root={}",
            scan.record.pid,
            scan.record.phase,
            scan.record
                .candidates
                .map_or_else(|| "?".to_string(), |count| count.to_string()),
            humanize_age(now.saturating_sub(scan.record.started_unix as i64)),
            scan.record.version,
            scan.record.root.display(),
        );
    }
    println!(
        "  A scan that fails normally removes its own record, so these died without unwinding \
         — most often an OOM kill. Acknowledge with `basemind comms doctor --clear-fatal`."
    );
}

#[cfg(all(feature = "comms", any(unix, windows)))]
fn cmd_comms_doctor(json: bool, probe: bool, clear_fatal: bool) -> Result<()> {
    use basemind::comms::singleton;
    use basemind::daemon_lock::{self, DaemonKind};

    if clear_fatal {
        return clear_recorded_failures(json);
    }

    // Read before the daemon rows so the report can say "nothing survived a scan" alongside "no
    // daemon is wedged". Filesystem-only, like the registry read — `doctor` must never need an RPC.
    let stale_scans = basemind::scan_evidence::stale_inflight_scans();

    let daemons = daemon_lock::live_daemons();
    let other_generations = daemon_lock::count_live_daemons_of_other_generations(DaemonKind::Comms);
    let ceiling = daemon_lock::max_live_daemons();
    let now = now_unix();

    // Probe only the comms family: a `shells` daemon left by an older release sits in the same registry
    // but does not speak this protocol on a comms socket, so a Status RPC there would be meaningless.
    let verdicts: Vec<Option<singleton::DaemonProbe>> = daemons
        .iter()
        .map(|record| {
            (probe && record.kind == DaemonKind::Comms)
                .then(|| singleton::probe_serving(&singleton::comms_socket_path(&record.dir)))
        })
        .collect();

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
                    "log": basemind::comms::singleton::daemon_log_path(&record.dir),
                });
                if let Some(verdict) = verdict {
                    row["reachable"] = probe_json(verdict);
                }
                row
            })
            .collect();
        let report = serde_json::json!({
            "count": daemons.len(),
            "other_generations": other_generations,
            "ceiling": ceiling,
            "over_ceiling": daemons.len() + other_generations > ceiling,
            "checked": if probe { "registry+probe" } else { "registry" },
            "daemons": items,
            "stale_scans": stale_scans.iter().map(|scan| serde_json::json!({
                "cache_dir": scan.cache_dir,
                "record": scan.record,
            })).collect::<Vec<_>>(),
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
            if record.kind == DaemonKind::Comms {
                println!(
                    "    log: {}",
                    basemind::comms::singleton::daemon_log_path(&record.dir).display()
                );
            }
        }
        if daemons.len() + other_generations > ceiling {
            println!(
                "WARNING: {} daemons exceed the ceiling of {ceiling}; run `basemind comms stop --all` to reclaim",
                daemons.len() + other_generations,
            );
        }
    }
    if other_generations > 0 {
        println!(
            "{other_generations} more comms daemon(s) belong to other basemind versions' protocol generations. \
             They count toward the ceiling; this version does not list, probe or stop them."
        );
    }

    report_stale_scans(&stale_scans, now);

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
        DaemonProbe::Serving { agent_store } => format!(" serving(agent store {agent_store})"),
        DaemonProbe::Failing { code, message } => format!(" LIVE BUT FAILING [{code}: {message}]"),
        DaemonProbe::Unreachable => " LIVE BUT NOT RESPONDING".to_string(),
    }
}

#[cfg(all(feature = "comms", any(unix, windows)))]
fn probe_json(verdict: &basemind::comms::singleton::DaemonProbe) -> serde_json::Value {
    use basemind::comms::singleton::DaemonProbe;
    match verdict {
        DaemonProbe::Serving { agent_store } => {
            serde_json::json!({ "state": "serving", "agent_store": agent_store })
        }
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
