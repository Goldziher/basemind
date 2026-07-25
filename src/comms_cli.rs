//! CLI helpers for the agent-comms broker lifecycle: the shell statusline snapshot and the
//! `basemind comms` lifecycle subcommands (daemon / start / stop / status). Extracted from
//! `main.rs` to keep the binary root under the module-size cap; behavior is unchanged. Most items
//! are gated on the `comms` feature; `cmd_statusline` compiles unconditionally and is a no-op
//! without it.

#[cfg(all(feature = "comms", any(unix, windows)))]
use anyhow::Context;
use anyhow::Result;

/// Print a compact statusline of the daemon's hot workspaces. Fast and silent by design: a missing
/// daemon (or any error) prints nothing and exits 0 so a shell statusline degrades cleanly. Without
/// the `comms` feature there is no daemon, so it is a no-op.
pub(crate) fn cmd_statusline() -> Result<()> {
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
pub(crate) fn cmd_comms(root: &std::path::Path, action: crate::CommsLifecycleCmd, json: bool) -> Result<()> {
    match action {
        crate::CommsLifecycleCmd::Daemon => basemind::cli::comms_daemon::run(),
        crate::CommsLifecycleCmd::Start => cmd_comms_start(),
        crate::CommsLifecycleCmd::Stop => cmd_comms_lifecycle_rpc(CommsRpc::Stop, json),
        crate::CommsLifecycleCmd::Status => cmd_comms_lifecycle_rpc(CommsRpc::Status, json),
        crate::CommsLifecycleCmd::Agent(cmd) => basemind::cli::comms::run(root, json, cmd),
    }
}

#[cfg(all(feature = "comms", any(unix, windows)))]
enum CommsRpc {
    Stop,
    Status,
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let agent = basemind::comms::identity::cli_agent_id(&root);
        let mut client = CommsClient::connect(&paths, agent, None, None)
            .await
            .map_err(|e| anyhow::anyhow!("connect to comms daemon: {e}"))?;
        match rpc {
            CommsRpc::Stop => {
                client.stop().await.map_err(|e| anyhow::anyhow!("stop: {e}"))?;
                if json {
                    println!("{{\"stopped\":true}}");
                } else {
                    println!("comms daemon stopping");
                }
            }
            CommsRpc::Status => {
                let status = client.status().await.map_err(|e| anyhow::anyhow!("status: {e}"))?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&status).map_err(|e| anyhow::anyhow!("serialize status: {e}"))?
                    );
                } else {
                    println!(
                        "pid={} version={} proto={} uptime={}s threads={} subscribers={}",
                        status.pid,
                        status.version,
                        status.proto_ver,
                        status.uptime_secs,
                        status.threads,
                        status.subscribers,
                    );
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
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
