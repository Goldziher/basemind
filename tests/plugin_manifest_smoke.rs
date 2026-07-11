use std::path::PathBuf;

use serde_json::Value;

#[test]
fn codex_mcp_launcher_should_resolve_from_plugin_root() {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let plugin_manifest_path = repository_root.join(".codex-plugin/plugin.json");
    let plugin_manifest: Value = serde_json::from_slice(
        &std::fs::read(&plugin_manifest_path).expect("read committed Codex plugin manifest"),
    )
    .expect("parse committed Codex plugin manifest");
    assert_eq!(
        plugin_manifest.get("mcpServers").and_then(Value::as_str),
        Some("./.mcp.json"),
        "Codex requires the MCP manifest at the plugin root",
    );

    let manifest_path = repository_root.join(".mcp.json");
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(&manifest_path).expect("read committed Codex MCP manifest"),
    )
    .expect("parse committed Codex MCP manifest");
    let basemind = manifest
        .get("mcpServers")
        .and_then(|servers| servers.get("basemind"))
        .expect("basemind MCP entry");

    assert_eq!(
        basemind.get("command").and_then(Value::as_str),
        Some("./scripts/mcp-launch.sh"),
        "Codex does not expand shell-style ${{PLUGIN_ROOT}} placeholders in MCP commands",
    );
    assert_eq!(
        basemind.get("cwd").and_then(Value::as_str),
        Some("."),
        "Codex must resolve the launcher relative to the installed plugin root",
    );
    assert!(
        repository_root.join("scripts/mcp-launch.sh").is_file(),
        "the configured Codex MCP launcher must be shipped with the plugin",
    );
}
