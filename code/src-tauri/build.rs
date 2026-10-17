fn main() {
    tauri_build::build();

    #[cfg(windows)]
    manifest_for_test_binaries();
}

/// Embed a Common Controls v6 application manifest into test binaries.
///
/// `tauri-build` stamps that manifest into the application binary, which is what
/// lets Windows resolve `TaskDialogIndirect` and the `*WindowSubclass` entry
/// points out of `comctl32.dll`. The window code pulls those imports in at link
/// time, and the loader resolves every import before `main` runs — without the
/// manifest the process loads the legacy v5.82 Common Controls, which does not
/// export them, and dies with `STATUS_ENTRYPOINT_NOT_FOUND` (0xc0000139).
///
/// `tauri-build` scopes its manifest to binary targets, so an integration test
/// that builds a real webview never starts. Repeating the dependency here for
/// test targets keeps `cargo test` runnable without touching the shipped
/// application binary.
#[cfg(windows)]
fn manifest_for_test_binaries() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("common-controls-v6.manifest");

    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
