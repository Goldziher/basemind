//! Cross-platform verification of the global cache path resolution.
//!
//! The index/blob cache is machine-global and lives under the platform-NATIVE data directory
//! (`directories::ProjectDirs::data_dir()`): `~/Library/Application Support/basemind` on macOS,
//! `%APPDATA%\basemind\data` on Windows, `$XDG_DATA_HOME` (or `~/.local/share/basemind`) on Linux.
//! Forcing the Linux XDG path on macOS/Windows would be a regression — `dirs`/`directories` returning
//! the native location there is CORRECT, not a bug to override. This test pins that per-platform, and
//! that every cache subtree routes through `cache_root()` and honors the `BASEMIND_DATA_HOME` seam.
//! It runs on the Linux/macOS/Windows CI matrix, on default features, so no heavy stack is required.

use std::path::Path;

use basemind::store::{cache_root, global_blobs_dir, workspace_cache_dir, workspace_key};

/// Deliberately the ONLY test function in this binary. `std::env::{set_var, remove_var}` are unsound
/// with a concurrent environment reader, and the integration-test harness runs distinct test fns on
/// separate threads; keeping this the sole fn guarantees no other thread touches the environment
/// while these mutations are in flight.
#[test]
fn cache_paths_are_platform_native_and_honor_the_data_home_override() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let saved_data_home = std::env::var_os("BASEMIND_DATA_HOME");

    // SAFETY: this is the only test fn in this binary, so no other thread reads or writes the
    unsafe {
        std::env::remove_var("BASEMIND_DATA_HOME");
    }

    let native = cache_root();
    assert!(
        native.is_absolute(),
        "cache_root() must be an absolute path, got {native:?}"
    );
    let native_str = native.to_string_lossy();
    assert!(
        native_str.contains("basemind"),
        "cache_root() must be namespaced under `basemind`, got {native:?}"
    );

    #[cfg(target_os = "macos")]
    {
        assert!(
            native_str.contains("Library/Application Support"),
            "macOS cache_root() must use the native Application Support dir, got {native:?}"
        );
        assert!(
            !native_str.contains(".local/share"),
            "macOS must NOT force the Linux XDG path, got {native:?}"
        );
    }
    #[cfg(target_os = "windows")]
    {
        assert!(
            native_str.contains("AppData"),
            "Windows cache_root() must use the native AppData dir, got {native:?}"
        );
        assert!(
            !native_str.contains(".local"),
            "Windows must NOT force the Linux XDG path, got {native:?}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        match std::env::var_os("XDG_DATA_HOME") {
            Some(xdg) if !xdg.is_empty() => assert!(
                native.starts_with(&xdg),
                "Linux cache_root() must honor $XDG_DATA_HOME ({xdg:?}), got {native:?}"
            ),
            _ => assert!(
                native_str.contains(".local/share"),
                "Linux cache_root() defaults under ~/.local/share, got {native:?}"
            ),
        }
    }

    assert!(
        global_blobs_dir().starts_with(&native),
        "the global blob store must live under cache_root(), got {:?}",
        global_blobs_dir()
    );
    assert!(
        workspace_cache_dir(manifest).starts_with(&native),
        "the per-workspace cache dir must live under cache_root(), got {:?}",
        workspace_cache_dir(manifest)
    );

    let key_a = workspace_key(manifest);
    let key_b = workspace_key(manifest);
    assert_eq!(key_a, key_b, "workspace_key must be deterministic for the same root");
    assert!(
        !key_a.is_empty() && key_a.chars().all(|c| c.is_ascii_hexdigit()),
        "workspace_key must be a non-empty hex digest, got {key_a:?}"
    );

    let temp = tempfile::tempdir().expect("tempdir");
    // SAFETY: as above — this is the sole test fn in this binary.
    unsafe {
        std::env::set_var("BASEMIND_DATA_HOME", temp.path());
    }
    assert_eq!(
        cache_root(),
        temp.path(),
        "BASEMIND_DATA_HOME must override cache_root() verbatim"
    );
    assert!(
        global_blobs_dir().starts_with(temp.path()),
        "the blob store must follow the BASEMIND_DATA_HOME override"
    );
    assert!(
        workspace_cache_dir(manifest).starts_with(temp.path()),
        "the per-workspace cache dir must follow the BASEMIND_DATA_HOME override"
    );

    // SAFETY: as above.
    unsafe {
        match saved_data_home {
            Some(value) => std::env::set_var("BASEMIND_DATA_HOME", value),
            None => std::env::remove_var("BASEMIND_DATA_HOME"),
        }
    }
}
