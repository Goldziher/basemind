//! Build script for `basemind-ui`.
//!
//! Only the `desktop` feature needs codegen: `tauri-build` reads `tauri.conf.json` and emits the
//! context (window config, capabilities, embedded icon) that `tauri::generate_context!` consumes at
//! compile time. Without `desktop` this is an empty `main`, so the default build (and
//! `cargo build --workspace` / CI) runs no Tauri build logic and pulls in none of its toolchain.
fn main() {
    #[cfg(feature = "desktop")]
    tauri_build::build();
}
