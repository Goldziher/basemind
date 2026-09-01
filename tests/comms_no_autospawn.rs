//! `BASEMIND_NO_AUTOSPAWN`: connect-only bring-up.
//!
//! The variable is the operator's only way to guarantee no daemon appears outside their
//! resource-controlled unit. `setsid` moves a child's session and process group but not its cgroup,
//! so a daemon auto-spawned from a shell is outside that unit's `MemoryMax` no matter what the code
//! does — see `docs/systemd/basemind-comms.service`. Suppressing the auto-spawn is the only lever
//! there is, which makes both halves of its contract load-bearing:
//!
//! * nothing spawns a daemon while it is set, and the refusal names the variable rather than
//!   failing later as an unexplained connection error, and
//! * a daemon that is ALREADY running is still used. Connect-only means *do not start one*, not
//!   *do not use one*; a version that also refused to talk to the operator's own supervised daemon
//!   would make the flag useless.
//!
//! This lives in its own binary on purpose. The check reads a process-global environment variable,
//! so setting it inside the lib test binary would race every other test that exercises the
//! bring-up path (`ensure_daemon_spawns_then_waits_for_ready`, among others) and flake them.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::cell::Cell;
use std::path::PathBuf;

use basemind::comms::singleton::{CommsPaths, NO_AUTOSPAWN_ENV, SingletonError, SpawnPolicy};

fn paths() -> CommsPaths {
    let dir = PathBuf::from("/tmp/basemind-no-autospawn-test");
    CommsPaths {
        socket_path: dir.join("comms.sock"),
        comms_dir: dir,
    }
}

/// Set for the whole binary: every test here wants it set, so no test can observe another's edit.
fn enable_opt_out() {
    // SAFETY: this binary contains only these tests, all of which want the same value, and none
    // spawns a thread that reads the environment concurrently with this write.
    unsafe { std::env::set_var(NO_AUTOSPAWN_ENV, "1") };
}

#[tokio::test]
async fn the_opt_out_refuses_to_spawn_and_names_itself() {
    enable_opt_out();
    let spawned = Cell::new(false);

    let error = basemind::comms::singleton::ensure_daemon_with(
        &paths(),
        |_| false,
        |_| {
            spawned.set(true);
            Ok(())
        },
    )
    .await
    .expect_err("connect-only must refuse rather than silently succeed with no daemon");

    assert!(!spawned.get(), "no daemon may be started while the opt-out is set");
    assert!(
        matches!(error, SingletonError::AutospawnDisabled { .. }),
        "the refusal must be its own case, not a timeout: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains(NO_AUTOSPAWN_ENV),
        "the message must name the variable that caused it: {message}"
    );
    assert!(message.contains("comms start"), "and the way out of it: {message}");
}

/// The other half. A running daemon is what the operator's unit provides; refusing to use it would
/// defeat the whole arrangement.
#[tokio::test]
async fn a_live_daemon_is_still_used_while_the_opt_out_is_set() {
    enable_opt_out();
    let spawned = Cell::new(false);

    basemind::comms::singleton::ensure_daemon_with(
        &paths(),
        |_| true,
        |_| {
            spawned.set(true);
            Ok(())
        },
    )
    .await
    .expect("an already-running daemon must be used, not refused");

    assert!(!spawned.get(), "a live daemon needs no spawn");
}

/// `basemind comms start` is the explicit operator intent the flag withholds from implicit callers.
/// Without this exemption a globally-set variable would leave no way to start the daemon by hand.
#[tokio::test]
async fn an_explicit_start_bypasses_the_opt_out() {
    enable_opt_out();
    let spawned = Cell::new(false);

    basemind::comms::singleton::ensure_daemon_with_policy(
        &paths(),
        SpawnPolicy::Explicit,
        |_| spawned.get(),
        |_| {
            spawned.set(true);
            Ok(())
        },
    )
    .await
    .expect("an explicit start must spawn regardless of the opt-out");

    assert!(spawned.get(), "`comms start` must still be able to start a daemon");
}
