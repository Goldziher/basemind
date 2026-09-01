use std::path::PathBuf;

use serde_json::Value;

/// Every harness manifest must launch the MCP server over **stdio**, through the shipped launcher.
///
/// The Claude and Cursor manifests used to point at `http://127.0.0.1:51786/mcp`, and a hard-coded
/// loopback port is not addressable to a *process*: whichever process binds it first is what the
/// client talks to, and no credential on the real daemon can fix a client that never reaches it. The
/// stdio relay is path-addressed and `0600`, so it closes the port-squatting hijack outright. The
/// launcher (`scripts/mcp-launch.sh`) resolves a version-matched binary and `exec`s `basemind serve`,
/// forwarding its arguments verbatim — the same wiring `gemini-extension.json` and
/// `kimi.plugin.json` already use.
///
/// The workspace root is passed **explicitly** rather than inherited from cwd. The HTTP form these
/// manifests replaced carried `?root=`, and issue #62 is what a cwd-derived root costs: a host
/// launched at `/` handed the daemon the whole filesystem. An expansion that silently fails is a
/// loud refusal from the root guard, where a wrong cwd would be a silent scan of the wrong tree.
#[test]
fn claude_and_cursor_manifests_launch_the_stdio_server() {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        repository_root.join("scripts/mcp-launch.sh").is_file(),
        "the launcher every stdio manifest points at must ship with the plugin",
    );

    for (manifest_rel, root_var, project_var) in [
        (".claude-plugin/plugin.json", "CLAUDE_PLUGIN_ROOT", "CLAUDE_PROJECT_DIR"),
        (".cursor-plugin/plugin.json", "CURSOR_PLUGIN_ROOT", "workspaceFolder"),
    ] {
        let path = repository_root.join(manifest_rel);
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|e| panic!("read {manifest_rel} ({e})")))
                .unwrap_or_else(|e| panic!("parse {manifest_rel} ({e})"));
        let server = manifest
            .get("mcpServers")
            .and_then(|servers| servers.get("basemind"))
            .unwrap_or_else(|| panic!("{manifest_rel} declares a basemind MCP server"));

        assert_eq!(
            server.get("command").and_then(Value::as_str),
            Some(format!("${{{root_var}}}/scripts/mcp-launch.sh").as_str()),
            "{manifest_rel} must exec the plugin-root launcher",
        );
        assert_eq!(
            server.get("args").and_then(Value::as_array).map(Vec::as_slice),
            Some(
                [
                    Value::from("serve"),
                    Value::from("--root"),
                    Value::from(format!("${{{project_var}}}")),
                ]
                .as_slice()
            ),
            "{manifest_rel} must launch the stdio `serve` transport against an explicit root",
        );
        assert!(
            server.get("url").is_none() && server.get("type").is_none(),
            "{manifest_rel} must not carry an HTTP transport: a hard-coded loopback port is \
             squattable and cannot be authenticated from the client side",
        );
        assert!(
            !manifest.to_string().contains("51786"),
            "{manifest_rel} must not hard-code the daemon's HTTP port anywhere",
        );
    }
}

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
