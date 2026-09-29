fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
        .with_debug_info(true);

    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");

    // Build scripts run on the *host*, so `cfg!(windows)` here would describe
    // the build machine, not the binary being built; check the target instead.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

/// Embeds the app icon (resource id 1, which Explorer/the taskbar pick up
/// and `tray.rs` loads for the tray icon) and version metadata into
/// `lanius-desktop.exe`.
fn embed_windows_resources() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let version = env!("CARGO_PKG_VERSION");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("ProductName", "Lanius")
        .set("FileDescription", "Lanius")
        .set("ProductVersion", version)
        .set("FileVersion", version)
        .set("LegalCopyright", "AGPL-3.0");
    res.compile()
        .expect("failed to embed Windows resources (icon/version info)");
}
