//! Ownership of `individual` notes across a server restart.

use super::*;

/// An `individual` note belongs to the workspace's durable owner, not to the session that saved
/// it: a server started again (a new session id) finds it.
#[tokio::test]
async fn an_individual_note_survives_a_server_restart() {
    let root = tempfile::tempdir().unwrap();
    let first = server(root.path(), "m");
    let owner = first.shared.memory_owner.clone();
    assert_ne!(owner, "test", "the owner is not the session id");
    let params = crate::mcp::types_memory::MemoryPutParams {
        visibility: Visibility::Individual,
        ..put_params("plan", "ship on friday", false)
    };
    crate::mcp::memory::run_memory_put(&first, params).await.unwrap();
    drop(first);

    let restarted = server(root.path(), "m");
    assert_eq!(restarted.shared.memory_owner, owner);
    let (vis_byte, namespace_owner) = crate::mcp::memory::memory_namespace(&restarted, Visibility::Individual);
    let note = crate::mcp::memory::memory_get_wire(&restarted, vis_byte, namespace_owner, "plan")
        .await
        .unwrap();
    assert!(note.is_some(), "the note saved before the restart is found");
}
