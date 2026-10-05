fn main() {
    // Embed the .exe icon (and any future Win32 resources) via windres/rc.exe.
    // No-op for host tools and non-Windows targets.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("assets/amty.rc", embed_resource::NONE);
    }
    println!("cargo:rerun-if-changed=assets/amty.rc");
    println!("cargo:rerun-if-changed=assets/amty-icon.ico");
}
