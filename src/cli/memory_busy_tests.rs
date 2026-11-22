/// A memory call on a session whose index another process holds exits `3` and names the daemon
/// as the fix, in process and over the daemon relay, by its busy mark alone.
#[test]
fn a_memory_call_without_the_index_exits_busy_and_names_the_daemon() {
    let refused = rmcp::ErrorData::from(crate::engine::memory::index_unavailable());
    let relayed: rmcp::ErrorData =
        serde_json::from_value(serde_json::to_value(&refused).expect("encode")).expect("decode");
    for error in [refused, relayed] {
        let error = crate::cli::run_tool("memory_save", Err(error)).expect_err("refused");
        assert_eq!(crate::cli::exit::exit_code_for(&error), crate::cli::exit::BUSY);
        assert_eq!(
            error.to_string(),
            "memory_save: memory: the memory_by_key index not available here: another basemind process \
             holds this workspace's index. Start the daemon (`basemind comms start`), which then serves \
             memory to every session, or stop the process that holds the lock, then retry."
        );
    }
}
