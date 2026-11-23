//! Embeds `assets/icon.ico` in the Windows executable, so Explorer, the Start menu and shortcuts show it. (The window
//! and taskbar icon are set at runtime in `main.rs`; the installer's icon comes from cargo-packager.)

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        embed_resource::compile("assets/gityas.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap_or_else(|err| panic!("cannot embed the Windows icon: {err}"));
    }
}
