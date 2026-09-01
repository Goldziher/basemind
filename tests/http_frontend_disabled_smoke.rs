//! The default: the daemon's HTTP MCP front-end binds nothing.
//!
//! Its own binary, deliberately. The grant is an environment variable, environment variables are
//! process-global, and `http_frontend_smoke.rs` sets it for every test in *that* binary — so the
//! not-granted case cannot be asserted alongside them without a race. Here nothing sets it.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;
use std::time::Duration;

use basemind::comms::daemon::Broker;
use basemind::comms::http_frontend;
use basemind::comms::store::CommsStore;

/// Without the operator grant, `serve_http` returns without binding a socket, without publishing a
/// portfile, and without failing — the daemon's UDS relay is unaffected. The fixed default port is
/// the one that matters: before this gate, every daemon on the machine held `127.0.0.1:51786` open
/// with the full tool surface (including `shell`) behind nothing but a `Host`-header check.
#[tokio::test]
async fn listener_does_not_bind_without_the_operator_grant() {
    basemind::store::init_isolated_cache();

    // SAFETY: nothing else in this binary touches these; the process is single-purpose.
    unsafe {
        std::env::remove_var(http_frontend::ALLOW_HTTP_ENV);
        // Pin an address of our own so the assertion below cannot be satisfied by an unrelated
        // process squatting the shared default port.
        std::env::set_var(http_frontend::HTTP_ADDR_ENV, "127.0.0.1:0");
    }

    let comms_dir = tempfile::tempdir().expect("comms tempdir");
    let store = Arc::new(CommsStore::open(comms_dir.path()).expect("open comms store"));
    let broker = Arc::new(Broker::new(store));

    // A portfile from an earlier, granted daemon must be cleared, not left advertising a dead port.
    let portfile = http_frontend::portfile_path(comms_dir.path());
    std::fs::write(&portfile, "127.0.0.1:51786\nstale-token\n").expect("plant a stale portfile");

    let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    // `serve_http` returns immediately rather than running an accept loop, so awaiting it directly
    // both proves that and avoids a task that would otherwise outlive the test.
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        http_frontend::serve_http(broker, comms_dir.path().to_path_buf(), shutdown_rx),
    )
    .await
    .expect("serve_http returns immediately when the front-end is not granted");
    assert!(outcome.is_ok(), "declining to serve is not an error: {outcome:?}");

    assert!(
        !portfile.exists(),
        "a stale portfile must be cleared, or discovery keeps pointing at a port nothing serves"
    );
    assert!(
        http_frontend::published_token(comms_dir.path()).is_none(),
        "no listener, no credential"
    );

    // With no portfile there is nothing for discovery to resolve, so readiness never succeeds.
    assert!(
        http_frontend::await_http_ready(comms_dir.path(), Duration::from_millis(200))
            .await
            .is_err(),
        "readiness cannot resolve a transport that was never started"
    );
}
