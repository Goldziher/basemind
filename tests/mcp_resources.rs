//! `resources/*` through the in-process rmcp client.

use rmcp::ServiceExt;
use rmcp::model::{
    ArgumentInfo, CallToolRequestParams, CompleteRequestParams, ContentBlock, ReadResourceRequestParams, Reference,
    ResourceContents,
};

async fn server() -> (tempfile::TempDir, rmcp::service::RunningService<rmcp::RoleClient, ()>) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "pub fn seed() {}\n").unwrap();
    std::fs::write(root.join("notes.md"), "# notes\n").unwrap();
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammars");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
            )
            .expect("scan");
        });
    });
    let transport = basemind::mcp::serve_in_memory(root, "working").await.expect("serve");
    let service = ().serve(transport).await.expect("handshake");
    (dir, service)
}

fn text_of(contents: &[ResourceContents]) -> String {
    match &contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resources_list_read_and_complete() {
    let (_dir, service) = server().await;

    let caps = service.peer_info().expect("info").capabilities.clone();
    assert!(caps.resources.is_some(), "resources capability must be advertised");

    let resources = service.list_all_resources().await.expect("list");
    let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
    assert!(
        uris.contains(&"basemind://status") && uris.contains(&"basemind://repo/map"),
        "{uris:?}"
    );
    let templates = service.list_all_resource_templates().await.expect("templates");
    assert!(templates.iter().any(|t| t.uri_template == "basemind://outline/{path}"));

    // Outline resource equals the code tool result.
    let outline = service
        .read_resource(ReadResourceRequestParams::new("basemind://outline/lib.rs"))
        .await
        .expect("read outline");
    let body = text_of(&outline.contents);
    assert!(body.contains("seed"), "{body}");
    let tool = service
        .call_tool(
            CallToolRequestParams::new("code").with_arguments(
                serde_json::json!({"mode": "outline", "path": "lib.rs"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("tool");
    let tool_body = tool
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap();
    let strip = |s: &str| {
        let mut v: serde_json::Value = serde_json::from_str(s).unwrap();
        v.as_object_mut().map(|o| o.remove("elapsed_us"));
        v
    };
    assert_eq!(strip(&body), strip(&tool_body));

    // Status and map read fine.
    let status = service
        .read_resource(ReadResourceRequestParams::new("basemind://status"))
        .await
        .expect("status");
    assert!(text_of(&status.contents).contains("file_count"));
    service
        .read_resource(ReadResourceRequestParams::new("basemind://repo/map"))
        .await
        .expect("map");

    // Policy and validation errors.
    // Markdown is code-mapped without the document tier, so only a `documents` build rejects it.
    #[cfg(feature = "documents")]
    assert!(
        service
            .read_resource(ReadResourceRequestParams::new("basemind://outline/notes.md"))
            .await
            .is_err(),
        "document-tier outline must error"
    );
    for bad in [
        "basemind://outline/../etc/passwd",
        "basemind://outline/%2e%2e/x",
        "basemind://memory/no-such-key",
        "basemind://bogus",
    ] {
        assert!(
            service
                .read_resource(ReadResourceRequestParams::new(bad))
                .await
                .is_err(),
            "{bad} must error"
        );
    }

    // Template argument completion.
    let done = service
        .complete(CompleteRequestParams::new(
            Reference::for_resource("basemind://outline/{path}"),
            ArgumentInfo::new("path", "li"),
        ))
        .await
        .expect("complete");
    assert!(
        done.completion.values.iter().any(|v| v == "lib.rs"),
        "{:?}",
        done.completion.values
    );

    let _ = service.cancel().await;
}
