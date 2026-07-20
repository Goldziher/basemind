use std::path::PathBuf;

use serde_json::Value;

#[test]
fn codex_mcp_should_launch_latest_release_from_workspace() {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let plugin_manifest_path = repository_root.join(".codex-plugin/plugin.json");
    let plugin_manifest: Value =
        serde_json::from_slice(&std::fs::read(&plugin_manifest_path).expect("read committed Codex plugin manifest"))
            .expect("parse committed Codex plugin manifest");
    assert_eq!(
        plugin_manifest.get("mcpServers").and_then(Value::as_str),
        Some("./.mcp.json"),
        "Codex requires the MCP manifest at the plugin root",
    );

    let manifest_path = repository_root.join(".mcp.json");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read committed Codex MCP manifest"))
            .expect("parse committed Codex MCP manifest");
    let basemind = manifest
        .get("mcpServers")
        .and_then(|servers| servers.get("basemind"))
        .expect("basemind MCP entry");

    assert_eq!(
        basemind.get("command").and_then(Value::as_str),
        Some("node"),
        "Codex must use the bundled locked launcher while inheriting the workspace cwd",
    );
    let args = basemind
        .get("args")
        .and_then(Value::as_array)
        .expect("Codex node launcher arguments");
    assert_eq!(args.first().and_then(Value::as_str), Some("-e"));
    assert_eq!(args.last().and_then(Value::as_str), Some("serve"));
    assert!(
        args.get(1)
            .and_then(Value::as_str)
            .is_some_and(|loader| loader.contains("codex-mcp-launch.mjs")),
        "Codex must locate and run the bundled latest-release launcher",
    );
    assert!(
        repository_root.join("scripts/codex-mcp-launch.mjs").is_file(),
        "the configured Codex bootstrap must be shipped with the plugin",
    );
    let bootstrap = std::fs::read_to_string(repository_root.join("scripts/codex-mcp-launch.mjs"))
        .expect("read Codex MCP bootstrap");
    assert!(
        bootstrap.contains("https://github.com/Goldziher/basemind/releases/latest"),
        "Codex must resolve the latest published release rather than a version range",
    );
    assert!(
        bootstrap.contains("BASEMIND_FORCE_VERSION"),
        "Codex must hand the resolved tag to the serialized release launcher",
    );
    let sync_script = std::fs::read_to_string(repository_root.join("scripts/sync-to-codex-plugin.sh"))
        .expect("read Codex plugin sync script");
    assert!(
        sync_script.contains("--include=\"/scripts/codex-mcp-launch.mjs\""),
        "the Codex plugin mirror must include the configured bootstrap",
    );
    assert!(
        basemind.get("cwd").is_none(),
        "Codex must inherit the consumer workspace cwd instead of indexing the plugin cache",
    );
    assert_eq!(
        basemind.get("startup_timeout_sec").and_then(Value::as_u64),
        Some(120),
        "the first release-backed launch needs enough time to install the binary",
    );
}
