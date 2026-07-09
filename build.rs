// Embed the application icon into the Windows executable so Explorer and the
// taskbar show it on the .exe itself. No-op for non-Windows targets (the
// runtime window icon in main.rs covers Linux/macOS).
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
}
