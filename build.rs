fn main() {
    println!("cargo:rerun-if-changed=schema/basemind-config-v1.schema.json");
    println!("cargo:rerun-if-changed=src/queries");
    link_macos_compiler_rt();
}

/// The prebuilt ONNX Runtime that `ort-sys` links statically uses Objective-C `@available` checks in
/// its `CoreML` provider, which lower to `___isPlatformVersionAtLeast` from clang's compiler-rt. rustc
/// links with `-nodefaultlibs`, so that archive is never pulled in and the release link fails. Link it
/// explicitly; a static archive only contributes the members that resolve undefined symbols, so this
/// is a no-op for builds without ONNX Runtime. The archive is asked of `cc`, the driver rustc links
/// with, so it matches the linker even when another `clang` (a Swift toolchain, say) leads `PATH`.
fn link_macos_compiler_rt() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(output) = std::process::Command::new("cc").arg("--print-resource-dir").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let resource_dir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let archive = std::path::Path::new(&resource_dir).join("lib/darwin/libclang_rt.osx.a");
    if archive.is_file() {
        // A link argument rather than `rustc-link-lib`, so the archive goes to the final link only
        // and is not bundled into the rlib next to `compiler_builtins`.
        println!("cargo:rustc-link-arg={}", archive.display());
    }
}
