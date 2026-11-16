//! Launches one real harness through the public API, denies every approval request and prints
//! the events. Opt-in: it does nothing unless `BASEMIND_AGENTS_LIVE=1`.
//!
//! ```text
//! BASEMIND_AGENTS_LIVE=1 cargo run -p basemind-agents --example live_probe -- \
//!     <claude|codex|opencode> <root> <permission: supervised|read-only> <brief> [mcp-server-command]
//! ```
//!
//! Run it only in a throwaway repository: the harness works there under the account it is
//! signed in to.

use std::time::Duration;

use basemind_agents::{
    AcpFlavor, AgentIdentity, Event, Harness, HarnessKind, LaunchConfig, LaunchRequest, Limits, McpServerSpec,
    PermissionMode, Registry,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("BASEMIND_AGENTS_LIVE").as_deref() != Some("1".as_ref()) {
        eprintln!("set BASEMIND_AGENTS_LIVE=1 to launch a real harness");
        return Ok(());
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [harness, root, mode, brief, rest @ ..] = args.as_slice() else {
        return Err("usage: live_probe <claude|codex|opencode> <root> <supervised|read-only> <brief> [mcp]".into());
    };
    let kind = match harness.as_str() {
        "claude" => HarnessKind::ClaudeCode,
        "codex" => HarnessKind::Codex,
        "opencode" => HarnessKind::Acp(AcpFlavor::OpenCode),
        other => return Err(format!("unknown harness {other}").into()),
    };
    let mut config = LaunchConfig::new(Harness::new(kind), root, AgentIdentity::new("live-probe")?);
    config.permission_mode = match mode.as_str() {
        "read-only" => PermissionMode::ReadOnly,
        _ => PermissionMode::Supervised,
    };
    config.limits = Limits {
        wall_time: Duration::from_secs(180),
        max_turns: 1,
        stop_grace: Duration::from_secs(5),
    };
    if let Some(command) = rest.first() {
        let mut server = McpServerSpec::basemind_serve(command);
        server.name = "stub".to_owned();
        server.args = Vec::new();
        config.mcp_server = Some(server);
    }
    let mut request = LaunchRequest::new(brief.clone());
    if kind == HarnessKind::ClaudeCode {
        request.model = Some("haiku".to_owned());
    }
    let session = Registry::with_defaults().launch(&config, &request)?;
    println!("pid {}", session.pid());
    for event in session.events() {
        match &event {
            Event::Text(text) => print!("{text}"),
            other => println!("\n{other:?}"),
        }
    }
    Ok(())
}
